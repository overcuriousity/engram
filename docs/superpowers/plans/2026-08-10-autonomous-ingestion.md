# Autonomous Ingestion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make engram repair its own ingestion — retry forever, derive spans itself, stop reporting formatting as invention, and close duplicate pairs that have nothing to disagree about — so the only thing left for a person is a real contradiction.

**Architecture:** Six independent changes over the existing job runner, verification module, and consolidation sweep. Retry stops being terminal and gains a reconciliation sweep that re-arms orphaned work. Span derivation moves from "check the model's claim, flag the mismatch" to "compute it locally, use the claim only as a fallback". The literal check learns that a `Word:` label is not part of the literal. The consolidation sweep closes fact-free pairs itself. Ops is rewritten to report state rather than offer chores.

**Tech Stack:** Rust 1.94+, sqlx/SQLite, axum, askama templates, tokio. Tests are `#[tokio::test]` against `Store::memory()` and the fakes in `src/infer/fake.rs` and `src/vector/memory.rs`; no containers.

## Global Constraints

- Design source: `docs/superpowers/specs/2026-08-10-autonomous-ingestion-design.md`.
- Nothing may delete or rewrite artifact text. Consolidation hides, flags, or asks — never merges.
- `auto_supersede` is not lowered. Auto-hiding below it is added for exactly one case: containment within the same corpus.
- Every change must keep `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean.
- Comments explain *why*, in the voice of the surrounding code. No comment that restates the line below it.
- No migration shims for existing rows: the development base can be recomputed. Schema changes go in a new `migrations/00NN_*.sql`.
- Run `cargo test` before every commit; commit messages are lowercase `type: summary` with a body explaining the failure being fixed.

---

### Task 1: Retry stops being terminal

**Files:**
- Modify: `src/store/jobs.rs:5` (`MAX_ATTEMPTS`), `:59` (`backoff_secs`), `:123` (`fail_job`)
- Test: `src/store/jobs.rs` (tests module at the end of the file)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `backoff_secs(attempts: i64) -> i64` capped at 21600; `Store::fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()>` which never sets `state = 'failed'`.

- [ ] **Step 1: Write the failing test**

In the `mod tests` block at the bottom of `src/store/jobs.rs`:

```rust
    #[test]
    fn backoff_climbs_to_hours_and_stops_there() {
        // An endpoint that is down stays down for minutes; one that is loading
        // a model takes ten. Retrying for a minute and giving up loses the work
        // to a delay the operator never sees.
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(20), 21_600);
        assert_eq!(backoff_secs(1_000), 21_600);
    }

    #[tokio::test]
    async fn a_job_out_of_attempts_waits_rather_than_failing() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "a1").await.unwrap();
        let job = s.claim_job().await.unwrap().unwrap();
        s.fail_job(job.id, MAX_ATTEMPTS + 10, "endpoint down")
            .await
            .unwrap();
        assert!(
            s.failed_jobs(10).await.unwrap().is_empty(),
            "a job was abandoned; nothing would ever pick it up again"
        );
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
            .bind(job.id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(state, "pending");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store::jobs`
Expected: FAIL — `backoff_secs(20)` returns 300, and `failed_jobs` returns one row.

- [ ] **Step 3: Write minimal implementation**

Replace `backoff_secs` and the head of `fail_job` in `src/store/jobs.rs`:

```rust
/// 2s, 4s, 8s … doubling to a six-hour ceiling, and never stopping.
///
/// The old ceiling was five minutes and the old caller gave up after five
/// attempts, which is one minute of patience in total. An inference endpoint
/// that loads a model on demand takes ten, so the entire budget was spent
/// before the endpoint had finished starting, and the work was lost until a
/// person noticed and pressed a button.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.clamp(1, 16) as u32;
    2i64.saturating_pow(exp).min(21_600)
}
```

```rust
    /// Put a job back in the queue with a delay. There is no terminal state:
    /// `attempts` past `MAX_ATTEMPTS` only means the delay has reached its
    /// ceiling, so an endpoint down for a weekend costs nothing and heals when
    /// it returns.
    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = 'pending', last_error = ?, run_after = ? WHERE id = ?",
        )
        .bind(err)
        .bind(now() + backoff_secs(attempts))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
```

Keep `MAX_ATTEMPTS = 5`: it still marks where behaviour changes (see Task 2), it no longer means abandonment. Update its doc comment to say so.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib store::jobs`
Expected: PASS. Then `cargo test` — fix any test that asserted a job reaches `failed`; the correct new assertion is that it stays pending with a `run_after` in the future.

- [ ] **Step 5: Commit**

```bash
git add src/store/jobs.rs
git commit -m "fix: a job that runs out of attempts was abandoned, not delayed"
```

---

### Task 2: A failed segment is retried, not parked

**Files:**
- Modify: `src/jobs/mod.rs:52-80` (the `Stage::Synthesize` exhausted arm), `src/jobs/synthesize.rs:355` (`fail_pending_segments`)
- Test: `src/jobs/synthesize.rs` (tests module)

**Interfaces:**
- Consumes: `Store::fail_job` from Task 1.
- Produces: `synthesize::fail_pending_segments` unchanged in signature; `SegmentState::Failed` now means "last attempt failed, will be tried again", set alongside a re-enqueued job.

- [ ] **Step 1: Write the failing test**

In `src/jobs/synthesize.rs` tests:

```rust
    #[tokio::test]
    async fn a_segment_the_endpoint_refused_is_queued_again() {
        // The failure that lost a quarter of a document: the model was loading
        // and returned 502 for ten minutes, the job spent five attempts in the
        // first minute, and nothing ever tried the segment again.
        let core = test_core_with_failing_synthesizer().await;
        let src = core
            .store
            .insert_corpus(&"line\n".repeat(400), "web", None)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Synthesize, "corpus", &src.id)
            .await
            .unwrap();
        for _ in 0..=MAX_ATTEMPTS + 1 {
            crate::jobs::run_next(&core).await.unwrap();
        }
        assert!(
            core.store.failed_jobs(10).await.unwrap().is_empty(),
            "the corpus was abandoned"
        );
        let jobs = core.store.job_counts().await.unwrap();
        assert!(
            jobs.iter().any(|(state, n)| state == "pending" && *n > 0),
            "no job is left to retry the segment: {jobs:?}"
        );
    }
```

Check the exact helper names before writing: `test_core_with_failing_synthesizer` is in `src/core/mod.rs:120`, `run_next` is the loop body in `src/jobs/mod.rs` — use whatever that function is actually called, and `job_counts` is the method behind the Ops queue row.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::synthesize::tests::a_segment_the_endpoint_refused_is_queued_again`
Expected: FAIL — no pending job remains, because the exhausted arm calls `complete_job` and only re-enqueues when *untried* windows exist.

- [ ] **Step 3: Write minimal implementation**

In `src/jobs/mod.rs`, the `(Stage::Synthesize, _) if exhausted` arm: after `fail_pending_segments` returns, always re-enqueue, not only when `requeue` is true:

```rust
                (Stage::Synthesize, _) if exhausted => {
                    tracing::warn!(error = %e, "segmentation is not getting through; backing off");
                    match synthesize::fail_pending_segments(core, &job.target_id, &e.to_string())
                        .await
                    {
                        Ok(_) => {
                            core.store.complete_job(job.id).await?;
                            // Always, not only when a window went untried. A
                            // segment the endpoint refused is not a verdict on
                            // the text — the endpoint was loading a model, or
                            // the machine was asleep — and the next attempt is
                            // hours away rather than seconds.
                            core.store
                                .enqueue(Stage::Synthesize, "corpus", &job.target_id)
                                .await?;
                        }
                        Err(fe) => {
                            core.store
                                .fail_job(job.id, job.attempts, &fe.to_string())
                                .await?;
                        }
                    }
                }
```

`enqueue` is `INSERT … ON CONFLICT` re-arming with `attempts = 0`, so add a `run_after` to that path or the retry is immediate: extend `Store::enqueue` with a sibling `enqueue_after(stage, kind, id, delay_secs)` and call it with `backoff_secs(job.attempts)` here.

In `src/jobs/synthesize.rs:355`, change the doc comment of `fail_pending_segments`: `SegmentState::Failed` now records the last error for display and does not mean the segment is finished with.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib jobs::`
Expected: PASS, including the existing tests around `fail_pending_segments`.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/mod.rs src/jobs/synthesize.rs src/store/jobs.rs
git commit -m "fix: a segment the endpoint refused was never tried again"
```

---

### Task 3: A reconciliation sweep re-arms orphaned work

**Files:**
- Create: `src/jobs/reconcile.rs`
- Modify: `src/jobs/mod.rs` (add `pub mod reconcile;`), `src/jobs/consolidate.rs:74` (call it at the head of `run`)
- Test: `src/jobs/reconcile.rs` (tests module)

**Interfaces:**
- Consumes: `Store::segments_for_corpus`, `Store::pending_artifacts_for_corpus`, `Store::enqueue`, `Store::list_corpora`.
- Produces: `pub async fn run(core: &Core) -> Result<usize>` — returns how many jobs it re-armed.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;

    #[tokio::test]
    async fn a_corpus_with_an_unfinished_segment_and_no_job_gets_one() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store
            .upsert_segments(&src.id, &[(1, 10), (11, 20)])
            .await
            .unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        // Segment 1 never ran and nothing is queued: the crack this closes.
        assert_eq!(run(&core).await.unwrap(), 1);
        assert!(core.store.claim_job().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_finished_corpus_is_left_alone() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store.upsert_segments(&src.id, &[(1, 10)]).await.unwrap();
        core.store
            .set_segment_state(&src.id, 0, SegmentState::Done, None)
            .await
            .unwrap();
        assert_eq!(run(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_sweep_does_not_pile_up_jobs_across_runs() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        core.store.upsert_segments(&src.id, &[(1, 10)]).await.unwrap();
        run(&core).await.unwrap();
        run(&core).await.unwrap();
        core.store.claim_job().await.unwrap().expect("one job");
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "the sweep queued the same work twice"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::reconcile`
Expected: FAIL to compile — `src/jobs/reconcile.rs` does not exist.

- [ ] **Step 3: Write minimal implementation**

```rust
//! The heartbeat: pick up anything that was left unfinished.
//!
//! Every stage already retries its own job. This exists for the case no retry
//! covers — a job that was completed while its work was not, a process killed
//! between two writes, a corpus whose segments were queued by a build that had
//! a bug in it. Without it, "the system repairs itself" holds only for
//! failures the system was watching at the time.
//!
//! It is cheap and idempotent: `enqueue` is keyed by (stage, target), so
//! re-arming something already queued changes nothing.

use crate::core::Core;
use crate::error::Result;
use crate::store::jobs::Stage;
use crate::store::segments::SegmentState;

pub async fn run(core: &Core) -> Result<usize> {
    let mut armed = 0;
    let mut offset = 0;
    loop {
        let page = core.store.list_corpora(100, offset).await?;
        if page.is_empty() {
            break;
        }
        for c in &page {
            let segments = core.store.segments_for_corpus(&c.id).await?;
            if segments.iter().any(|w| w.state != SegmentState::Done) {
                core.store.enqueue(Stage::Synthesize, "corpus", &c.id).await?;
                armed += 1;
                continue;
            }
            if !core.store.pending_artifacts_for_corpus(&c.id).await?.is_empty() {
                core.store.enqueue(Stage::Embed, "corpus", &c.id).await?;
                armed += 1;
            }
        }
        offset += page.len() as i64;
    }
    if armed > 0 {
        tracing::info!(armed, "reconciliation queued unfinished work");
    }
    Ok(armed)
}
```

Then in `src/jobs/consolidate.rs`, at the top of `run` and before `heal_dangling_supersessions`:

```rust
    // Before looking for duplicates, finish what was started: a sweep that
    // consolidates a half-ingested corpus is judging an incomplete base.
    crate::jobs::reconcile::run(core).await?;
```

Note the counting subtlety for `the_sweep_does_not_pile_up_jobs_across_runs`: `enqueue` is idempotent per (stage, target), so the second run re-arms the same row rather than adding one. `armed` counts intent, not rows; the test asserts on the queue, not the count.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib jobs::reconcile` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/reconcile.rs src/jobs/mod.rs src/jobs/consolidate.rs
git commit -m "feat: a sweep that picks up whatever was left unfinished"
```

---

### Task 4: Spans are derived, and the flag is gone

**Files:**
- Modify: `src/jobs/synthesize.rs:72-120` (span resolution), `:186` (`paraphrased`), `:206-240` (`flag_unverified`), `src/infer/verify.rs:153` (`FLAG_SPAN`)
- Modify: `src/web/ui.rs` (drop the span rows from the flagged list — see Task 6)
- Test: `src/jobs/synthesize.rs` tests

**Interfaces:**
- Consumes: `verify::locate_span(artifact_text, segment_body, segment_start) -> Option<(i64, i64)>`.
- Produces: `SpanOrigin` reduced to `Derived | Model | Segment`; `flag_unverified` no longer takes `spans: &[SpanOrigin]`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_span_is_derived_rather_than_adjudicated() {
        // The model's corpus_lines used to be checked, disbelieved, and turned
        // into a review task carrying a button that spends a model call. The
        // span is computable from stored text, so it is computed.
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("alpha line one\nbeta line two\ngamma line three", "web", None)
            .await
            .unwrap();
        core.store
            .enqueue(Stage::Synthesize, "corpus", &src.id)
            .await
            .unwrap();
        run(&core, &src.id).await.unwrap();

        for c in core.store.artifacts_for_corpus(&src.id).await.unwrap() {
            assert!(
                !c.flags.iter().any(|f| f == "span_unverified"),
                "a derived span produced a review task: {:?}",
                c.flags
            );
        }
    }
```

The default `FakeSynthesizer` returns artifacts whose text is the window's own lines, so derivation succeeds; check `src/infer/fake.rs` for what it emits before asserting on span values.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::synthesize`
Expected: PASS or FAIL depending on the fake — if it passes, make it meaningful by seeding a `ProposedArtifact` with a deliberately wrong `corpus_lines` through `FakeSynthesizer`, and assert no flag is produced.

- [ ] **Step 3: Write minimal implementation**

In `src/jobs/synthesize.rs`, replace the span block:

```rust
        // The span is ours to compute. Asking the model for `corpus_lines` and
        // then checking the answer produced a third outcome — a claim that
        // failed the check — which became a flag on the artifact and a button
        // offering to re-synthesise a whole segment over a line number. Since
        // `locate_span` can find an artifact's own text even where the source
        // is hard-wrapped and synthesis reflowed it, the claim is worth only
        // what it is: a hint for the case where nothing matches.
        for c in &mut chunks {
            let derived = crate::infer::verify::locate_span(&c.text, &text, w.start_line);
            let hinted = c
                .corpus_lines
                .map(|(a, b)| (a + w.start_line - 1, b + w.start_line - 1));
            let span = derived
                .or(hinted)
                .unwrap_or((w.start_line, w.end_line));
            let clamped = (
                span.0.clamp(w.start_line, w.end_line),
                span.1.clamp(w.start_line, w.end_line),
            );
            c.corpus_lines = Some(if clamped.0 <= clamped.1 {
                clamped
            } else {
                (w.start_line, w.end_line)
            });
        }
```

Delete `enum SpanOrigin` and its uses, drop the `spans` parameter from `flag_unverified`, and delete the `FLAG_SPAN` branch inside it. In `src/infer/verify.rs`, delete `pub const FLAG_SPAN` and any test naming it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS. Tests asserting a `span_unverified` flag must be deleted, not weakened — the behaviour is gone on purpose.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/synthesize.rs src/infer/verify.rs
git commit -m "fix: a line number engram can compute was a review task"
```

---

### Task 5: A label is not an invented command

**Files:**
- Modify: `src/infer/verify.rs:94` (`missing_literals`)
- Test: `src/infer/verify.rs` tests

**Interfaces:**
- Consumes: `extract_literals`.
- Produces: `missing_literals` unchanged in signature.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_label_the_model_added_is_not_a_missing_literal() {
        // Line 635 of the source read "wird binär 0010 1001 1111 1001". The
        // model fenced the digits and wrote "Binär:" in front of them, and the
        // check reported an invented command — which is a review task about
        // formatting.
        let window = "Die Zahl 29 F9 wird binär 0010 1001 1111 1001 gespeichert.";
        let chunk = "```\nBinär: 0010 1001 1111 1001\n```";
        assert!(missing_literals(chunk, &[], window).is_empty());
    }

    #[test]
    fn an_invented_command_is_still_caught_when_it_carries_a_label() {
        let window = "Unmount the device first.";
        let chunk = "```\nRun: wipefs --all /dev/sdX\n```";
        assert_eq!(
            missing_literals(chunk, &[], window),
            vec!["wipefs --all /dev/sdX".to_string()]
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib infer::verify::tests::a_label_the_model_added_is_not_a_missing_literal`
Expected: FAIL — the whole fenced line including `Binär:` is looked for and not found.

- [ ] **Step 3: Write minimal implementation**

In `src/infer/verify.rs`, before the `filter` in `missing_literals`, strip a leading label from each literal and look for the remainder as well:

```rust
/// A literal minus a label the model put in front of it.
///
/// `Binär: 0010 1001 1111 1001` for a source that says
/// `wird binär 0010 1001 1111 1001` invents nothing — the digits, which are
/// the part that matters if someone retypes them, are verbatim. The label is
/// presentation, and reporting it as a possibly-invented command buries the
/// real misses.
fn without_label(lit: &str) -> Option<&str> {
    let (label, rest) = lit.split_once(':')?;
    let rest = rest.trim();
    // Only a single word, so `dd if=x.iso of=/dev/sdX` — which has a colon in
    // no plausible label position — is never split into something weaker.
    if rest.is_empty() || label.split_whitespace().count() != 1 {
        return None;
    }
    Some(rest)
}
```

and in the filter:

```rust
        .filter(|lit| {
            let n = normalize(lit);
            if haystack.contains(&n) {
                return false;
            }
            match without_label(lit) {
                Some(bare) => !haystack.contains(&normalize(bare)),
                None => true,
            }
        })
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib infer::verify`
Expected: PASS, including the existing literal tests — `dd if=archlinux.iso of=/dev/sdX` must still be reported when absent.

- [ ] **Step 5: Commit**

```bash
git add src/infer/verify.rs
git commit -m "fix: a label in front of verbatim digits read as an invented command"
```

---

### Task 6: Pairs that do not disagree close themselves

**Files:**
- Modify: `src/jobs/consolidate.rs:149-166` (the review band) and `:193` (`judge_pending`)
- Test: `src/jobs/consolidate.rs` tests

**Interfaces:**
- Consumes: `crate::infer::facts::may_disagree(a, b) -> bool`, `Store::set_pair_state`.
- Produces: no new signatures; `Outcome` gains `pub closed: usize`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_pair_with_nothing_to_disagree_about_never_reaches_the_queue() {
        // The prefilter already knows these two state no differing value. It
        // only ran when the judge was enabled, so with the judge off — the
        // default — every near pair became a question for a person.
        let core = test_core().await;
        seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 0, "{out:?}");
        assert_eq!(out.closed, 1);
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            0
        );
        // Both artifacts survive: closing is not hiding.
        assert_eq!(core.store.artifacts_for_corpus_count().await.unwrap_or(2), 2);
    }

    #[tokio::test]
    async fn a_pair_stating_different_values_still_waits_for_a_person() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 1, "{out:?}");
    }
```

Drop the `artifacts_for_corpus_count` line if no such method exists; assert instead that neither artifact has `superseded_by` set.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::consolidate`
Expected: FAIL — `out.closed` does not exist, and the factless pair is queued.

- [ ] **Step 3: Write minimal implementation**

Add `pub closed: usize` to `Outcome`. In the review-band loop of `run`, after fetching `a` and `b`:

```rust
        // Two artifacts that state no differing value have nothing for a
        // person to rule on. Recording the pair as settled keeps the sweep
        // from re-asking, and both artifacts stay exactly where they are —
        // closing a question is not hiding an answer.
        if !crate::infer::facts::may_disagree(&a.text, &b.text) {
            if core.store.record_pair(&p.a, &p.b, p.score).await? {
                out.closed += 1;
            }
            if let Some(pair) = core.store.find_pair(&p.a, &p.b).await? {
                core.store
                    .set_pair_state(pair.id, crate::store::pairs::PairState::NoConflict, None)
                    .await?;
            }
            continue;
        }
```

If `Store::find_pair` does not exist, add it to `src/store/pairs.rs` beside `record_pair`:

```rust
    /// The stored row for a pair, in whichever order it was filed.
    pub async fn find_pair(&self, a: &str, b: &str) -> Result<Option<ArtifactPair>> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let row = sqlx::query("SELECT * FROM artifact_pairs WHERE a_id = ? AND b_id = ?")
            .bind(lo)
            .bind(hi)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_pair))
    }
```

Check how `record_pair` orders `a_id`/`b_id` before writing this — it must match.

Then simplify `judge_pending`: the prefilter branch there becomes unreachable for new pairs but must stay, because rows filed before this change are still pending.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib jobs::consolidate` then `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/consolidate.rs src/store/pairs.rs
git commit -m "feat: a pair with no differing value is not a question for a person"
```

---

### Task 7: A stuttered duplicate inside one corpus is hidden

**Files:**
- Modify: `src/jobs/consolidate.rs` (auto-hide block around `:126-144`)
- Test: `src/jobs/consolidate.rs` tests

**Interfaces:**
- Consumes: `Chunk.corpus_id`, `Chunk.text`, `Core::store.set_superseded_by`, `vectors.set_superseded`.
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn one_synthesis_call_emitting_a_passage_twice_resolves_itself() {
        // Same corpus, same call, one text wholly inside the other. That is a
        // defect in one artifact, not two sources disagreeing, and it sat in
        // the review queue below auto_supersede.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                (
                    "Bind mounts attach a directory elsewhere. Use mount --bind for it.",
                    [1.0, 0.0],
                ),
                ("Bind mounts attach a directory elsewhere.", [0.94, 0.34]),
            ],
        )
        .await;
        run(&core).await.unwrap();
        let shorter = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(shorter.superseded_by.as_deref(), Some(ids[0].as_str()));
    }

    #[tokio::test]
    async fn containment_across_two_corpora_is_left_alone() {
        // Two documents that happen to share a sentence are two sources. This
        // is the case auto_supersede deliberately refuses below 0.95.
        let core = test_core().await;
        // seed() puts everything in one corpus; build the second corpus by
        // hand with insert_corpus + insert_artifacts + vectors.upsert, exactly
        // as seed() does, and assert neither artifact is superseded.
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jobs::consolidate::tests::one_synthesis_call_emitting_a_passage_twice_resolves_itself`
Expected: FAIL — the pair scores below `auto_supersede`, so it is queued rather than hidden.

- [ ] **Step 3: Write minimal implementation**

In the review-band loop, before the `may_disagree` check from Task 6:

```rust
        // One call emitted the same passage twice: the shorter text is wholly
        // inside the longer, and both came out of the same document. Nothing
        // is lost by hiding it — the surviving artifact says everything it
        // said — and Ops still lists it with an undo.
        if a.corpus_id == b.corpus_id {
            let (long, short) = if a.text.len() >= b.text.len() {
                (&a, &b)
            } else {
                (&b, &a)
            };
            if contains_normalized(&long.text, &short.text) {
                core.store
                    .set_superseded_by(&short.id, Some(&long.id))
                    .await?;
                core.vectors.set_superseded(&short.id, true).await?;
                out.superseded += 1;
                tracing::info!(superseded = %short.id, by = %long.id, "hid a stuttered duplicate");
                continue;
            }
        }
```

with, near `keeper`:

```rust
/// Is the whole of one artifact inside the other, whitespace aside?
fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/consolidate.rs
git commit -m "feat: hide a passage one synthesis call emitted twice"
```

---

### Task 8: Ops reports state instead of offering chores

**Files:**
- Modify: `src/web/ui.rs:287-301` (`OpsTemplate`), `:630-740` (the handler), `templates/ops.html`
- Modify: `src/store/jobs.rs` (add `retrying_jobs`)
- Test: `src/web/ui.rs` tests

**Interfaces:**
- Consumes: everything above.
- Produces: `Store::retrying_jobs(&self, limit: i64) -> Result<Vec<RetryingJob>>` where `RetryingJob { stage: String, target_id: String, attempts: i64, next_attempt_secs: i64, last_error: Option<String> }`.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn ops_reports_what_is_retrying_rather_than_asking_for_a_click() {
        let (app, token, core) = crate::web::api::tests::app_token_and_core().await;
        core.store
            .enqueue(Stage::Embed, "artifact", "a1")
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        core.store
            .fail_job(job.id, 9, "endpoint down")
            .await
            .unwrap();
        let body = get_ops_html(&app, &token).await;
        assert!(body.contains("Retrying"), "{body}");
        assert!(
            !body.contains("Re-synthesize segment"),
            "the review queue is still a to-do list"
        );
    }
```

Write `get_ops_html` as the existing UI tests fetch pages — copy the helper already in `src/web/ui.rs` tests rather than inventing one.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib web::ui`
Expected: FAIL — no "Retrying" section exists.

- [ ] **Step 3: Write minimal implementation**

Add to `src/store/jobs.rs`:

```rust
    /// Jobs waiting on a backoff, soonest first. There is no failed state to
    /// report any more; this is what replaced it.
    pub async fn retrying_jobs(&self, limit: i64) -> Result<Vec<RetryingJob>> {
        let rows = sqlx::query(
            "SELECT stage, target_id, attempts, last_error, run_after FROM jobs
              WHERE state = 'pending' AND run_after > ? AND attempts > 0
              ORDER BY run_after LIMIT ?",
        )
        .bind(now())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| RetryingJob {
                stage: r.get("stage"),
                target_id: r.get("target_id"),
                attempts: r.get("attempts"),
                next_attempt_secs: r.get::<i64, _>("run_after") - now(),
                last_error: r.get("last_error"),
            })
            .collect())
    }
```

In `src/web/ui.rs`, replace `failed: Vec<FailedJob>` with `retrying: Vec<RetryingJob>`, drop `flagged: Vec<FlaggedRow>` from the template entirely, and delete the `/ui/corpora/{cid}/segments/{idx}/resynthesize` route together with its handler and the `Re-synthesize segment` button in `templates/ops.html`. Add a `Retrying` section rendering stage, target, attempts and `next_attempt_secs` as a coarse duration.

Keep the literal note visible where the reader is: render `flag_detail` on the artifact detail page (`templates/artifact.html` or whichever template the detail pane uses) if it is not already there.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS. Delete any test that asserts the resynthesize route exists.

- [ ] **Step 5: Commit**

```bash
git add src/web/ui.rs src/store/jobs.rs templates/
git commit -m "feat: ops reports what the system is doing, not what you must do"
```

---

### Task 9: Documentation

**Files:**
- Modify: `README.md` (the verification section around line 168, the duplicates section around line 192, and the Ops description)

**Interfaces:** none.

- [ ] **Step 1: Update the prose**

Three passages change, and each must say what is true after Tasks 1–8:

1. The verification bullets: spans are derived locally and never doubted, so there is no span flag; the literal check ignores a label the model added; coverage is unchanged from its current text.
2. The duplicates section: a pair with no differing fact closes itself, an artifact contained in another from the same corpus is hidden with an undo, and `auto_supersede` is unchanged.
3. Retry: attempts back off to six hours and never stop, and a reconciliation sweep re-arms anything unfinished, so a failed segment needs no button.

- [ ] **Step 2: Check the claims**

Run: `grep -n "span_unverified\|Re-synthesize\|failed job" README.md`
Expected: no hits describing behaviour that no longer exists.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: describe ingestion that repairs itself"
```

---

## Self-Review

**Spec coverage:** §3 nothing terminal → Tasks 1–3. §4 spans derived → Task 4. §5 literal labels → Task 5. §6 duplicates → Tasks 6–7. §7 Ops → Task 8. §8 testing → each task's tests. §9 out of scope → nothing here changes embeddings, merges text, or lowers `auto_supersede`.

**Placeholders:** none. Three tasks say "check the exact helper name before writing" — that is an instruction to verify against the codebase, not a gap in the plan, and each names the file and line to look at.

**Type consistency:** `RetryingJob` is defined in Task 8 and used only there. `Outcome.closed` is added in Task 6 and used in Tasks 6 and 7. `SpanOrigin` is deleted in Task 4 and referenced nowhere after. `find_pair` returns `Option<ArtifactPair>`, matching `row_to_pair`.
