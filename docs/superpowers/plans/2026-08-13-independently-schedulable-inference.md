# Independently Schedulable Inference — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every inference call in engram an independently schedulable queue unit with its own attempt budget and backoff, paced by one global gate that also carries the interactive lane and a circuit breaker.

**Architecture:** A job becomes one inference call. `Synthesize` and `Consolidate` stop making calls and become local work that *arms* units (`SegmentWindow`, `Title`, `Judge`, `Embed`). Claim ordering gains a `seq` column so units round-robin across documents. One `InferenceGate` in `Core` sits in front of every call site.

**Tech Stack:** Rust 2024 (rustc 1.94), tokio, sqlx/SQLite, async-trait, tracing. Tests use `#[tokio::test]`, `tokio::time::pause`, and the fakes in `src/infer/fake.rs`.

**Spec:** `docs/superpowers/specs/2026-08-13-independently-schedulable-inference-design.md`

## Global Constraints

- `MAX_ATTEMPTS` stays `5`; it now means five attempts at **one unit**.
- Backoff is unchanged: `backoff_secs`, doubling to a 21,600s ceiling.
- Circuit breaker: **3** consecutive `Error::Inference` failures; probe interval **60s**. `Error::MalformedLlmOutput` must **not** trip it.
- `workers = 1`. Nothing in this plan may introduce concurrency between units.
- `migrate()` runs `schema.sql` on every connect and **cannot alter a table**. Adding a column is safe; dropping one is not. `segments.attempts` is therefore left in place and unused, never dropped.
- `CREATE INDEX IF NOT EXISTS` on an existing name is a silent no-op — index changes require `DROP INDEX IF EXISTS` by the old name first.
- Every task ends green: `cargo test`, `cargo clippy --all-targets` (no warnings), `cargo fmt --check`.
- Commit at the end of every task. Branch is `feat/independently-schedulable-inference`.

## Task 0 (operator, not code): settle window size

Spec §10. Not a coding task and **not a blocker** for Tasks 1–10 — the unit is one window and one call whether a window is 512 or 2048 tokens. Run it before tuning the scheduler.

Baselines already recorded: coverage `0.5539`, literal-flag rate `8 of 277 (2.9%)` on corpus `019ff75a-61b1-7703-aea9-f2a3ae9a0ddd`.

Re-ingest that document at `output_ratio` 8.0, 16.0, 32.0. Capture coverage, flag rate, malformed-output count (from a clean run — the 2026-08-12 journal double-counts via cache replays), and eval `recall@k`/MRR. **Decision rule:** adopt the largest ratio at which malformed-output count is near zero and coverage does not regress.

## File Structure

| File | Responsibility |
|---|---|
| `src/infer/gate.rs` | **new** — `InferenceGate`: cooldown, interactive lease, circuit breaker |
| `src/jobs/window.rs` | **new** — the `SegmentWindow` handler and the settle rule |
| `src/jobs/judge.rs` | **new** — the `Judge` handler, moved out of `consolidate.rs` |
| `src/infer/mod.rs` | add `pub mod gate;` |
| `src/config.rs` | global `cooldown_secs`, breaker settings |
| `src/store/schema.sql` | `jobs.seq`, rebuilt claim index |
| `src/store/jobs.rs` | new `Stage` variants, `seq`, claim ordering, unit-target helpers |
| `src/jobs/mod.rs` | dispatch for the new stages |
| `src/jobs/synthesize.rs` | shrinks to planning + settle; loses the window loop |
| `src/jobs/embed.rs` | one batch per run, re-arms |
| `src/jobs/consolidate.rs` | local only; arms `Judge` units |
| `src/jobs/reconcile.rs` | arms per-window units; the migration path |
| `src/core/mod.rs` | holds `Arc<InferenceGate>` |
| `src/core/ask.rs` | holds an interactive lease |

`synthesize.rs` is already ~1500 lines. Moving the window handler to `window.rs` is a split by responsibility, not a unilateral restructure.

---

### Task 1: The inference gate

**Files:**
- Create: `src/infer/gate.rs`
- Modify: `src/infer/mod.rs` (add `pub mod gate;`)
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `InferenceGate::new(cooldown: Duration) -> InferenceGate`; `.with_breaker(after: usize, probe: Duration) -> InferenceGate`; `async fn background(&self)`; `fn interactive(self: &Arc<Self>) -> InteractiveLease`; `fn call_succeeded(&self)`; `fn call_failed(&self, e: &Error)`. `InteractiveLease` releases on `Drop`.

- [ ] **Step 1: Write the failing tests**

Create `src/infer/gate.rs` with only the test module and the `use` lines:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn gate(cooldown_secs: u64) -> Arc<InferenceGate> {
        Arc::new(InferenceGate::new(Duration::from_secs(cooldown_secs)))
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_gate_with_no_cooldown_lets_work_through_at_once() {
        let g = gate(0);
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cooldown_is_measured_from_when_the_last_call_ended() {
        let g = gate(5);
        g.call_succeeded();
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn background_work_waits_while_an_interactive_call_holds_a_lease() {
        let g = gate(0);
        let lease = g.interactive();
        let g2 = Arc::clone(&g);
        let waiter = tokio::spawn(async move { g2.background().await });

        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(!waiter.is_finished(), "background ran while an ask was in flight");

        drop(lease);
        waiter.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_interactive_call_never_waits_even_during_a_cooldown() {
        // The point of the lane: a person is waiting, and the pacer exists to
        // protect the GPU from batch work, not from them.
        let g = gate(600);
        g.call_succeeded();
        let started = tokio::time::Instant::now();
        let _lease = g.interactive();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn three_transport_failures_in_a_row_open_the_breaker() {
        let g = Arc::new(
            InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)),
        );
        for _ in 0..3 {
            g.call_failed(&Error::Inference { role: "chunk", detail: "502".into() });
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_closes_the_breaker_again() {
        let g = Arc::new(
            InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)),
        );
        for _ in 0..2 {
            g.call_failed(&Error::Inference { role: "chunk", detail: "502".into() });
        }
        g.call_succeeded();
        g.call_failed(&Error::Inference { role: "chunk", detail: "502".into() });

        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO, "the run was not reset");
    }

    #[tokio::test(start_paused = true)]
    async fn unreadable_output_is_not_an_endpoint_failure() {
        // The distinction the whole design rests on: the model answered, so the
        // endpoint is fine. Tripping the breaker here would stop the queue over
        // one document's punctuation.
        let g = Arc::new(
            InferenceGate::new(Duration::ZERO).with_breaker(3, Duration::from_secs(60)),
        );
        for _ in 0..5 {
            g.call_failed(&Error::MalformedLlmOutput("duplicate field `tags`".into()));
        }
        let started = tokio::time::Instant::now();
        g.background().await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib infer::gate`
Expected: FAIL to compile — `InferenceGate` not found.

- [ ] **Step 3: Implement the gate**

Prepend to `src/infer/gate.rs`:

```rust
//! One pacer in front of every inference call.
//!
//! Three jobs that all answer the same question — may a background call start
//! now? The cooldown protects a desktop GPU from unbroken load. The interactive
//! lease keeps the worker from piling work onto the endpoint while a person is
//! waiting on `ask`. The breaker stops thirty-four units from each spending a
//! fifteen-minute timeout discovering the same dead endpoint.
//!
//! It sits around calls rather than around jobs, so the two stages that make no
//! inference call — planning a corpus into windows, and the consolidation sweep
//! — never wait for a cooldown they did not earn.

use crate::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

#[derive(Default)]
struct GateState {
    /// Interactive calls in flight. Background work waits while this is above
    /// zero; it is a count rather than a flag because `ask` makes more than one
    /// call and the UI can have two queries open.
    interactive: usize,
    last_finished: Option<Instant>,
    consecutive_transport_failures: usize,
    breaker_open_until: Option<Instant>,
}

pub struct InferenceGate {
    state: Mutex<GateState>,
    resumed: Notify,
    cooldown: Duration,
    breaker_after: usize,
    probe_after: Duration,
}

impl InferenceGate {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            resumed: Notify::new(),
            cooldown,
            // Off unless configured, so a test that does not ask for a breaker
            // cannot be surprised by one.
            breaker_after: usize::MAX,
            probe_after: Duration::ZERO,
        }
    }

    pub fn with_breaker(mut self, after: usize, probe: Duration) -> Self {
        self.breaker_after = after.max(1);
        self.probe_after = probe;
        self
    }

    /// Returns when a background inference call may start.
    pub async fn background(&self) {
        loop {
            // The lock is never held across an await: the wait is computed,
            // dropped, and only then slept on.
            let wait = {
                let st = self.state.lock().expect("gate state");
                if st.interactive > 0 {
                    None
                } else {
                    let now = Instant::now();
                    let ready_at = [
                        st.last_finished.map(|t| t + self.cooldown),
                        st.breaker_open_until,
                    ]
                    .into_iter()
                    .flatten()
                    .max();
                    match ready_at {
                        Some(t) if t > now => Some(t - now),
                        _ => return,
                    }
                }
            };
            match wait {
                Some(d) => tokio::time::sleep(d).await,
                // Held off by an interactive call, which has no deadline. The
                // lease wakes us when it drops.
                None => self.resumed.notified().await,
            }
        }
    }

    /// A lease for an interactive call. Returns immediately, always.
    pub fn interactive(self: &Arc<Self>) -> InteractiveLease {
        self.state.lock().expect("gate state").interactive += 1;
        InteractiveLease {
            gate: Arc::clone(self),
        }
    }

    pub fn call_succeeded(&self) {
        let mut st = self.state.lock().expect("gate state");
        st.last_finished = Some(Instant::now());
        st.consecutive_transport_failures = 0;
        st.breaker_open_until = None;
    }

    /// A failed call still occupied the GPU, so it still starts the cooldown.
    /// Only a transport failure counts toward the breaker: `MalformedLlmOutput`
    /// means the endpoint answered and this window's text is the problem.
    pub fn call_failed(&self, e: &Error) {
        let mut st = self.state.lock().expect("gate state");
        st.last_finished = Some(Instant::now());
        if !matches!(e, Error::Inference { .. }) {
            return;
        }
        st.consecutive_transport_failures += 1;
        if st.consecutive_transport_failures >= self.breaker_after {
            st.breaker_open_until = Some(Instant::now() + self.probe_after);
            // Reset so one further failure after the probe re-opens it, rather
            // than every subsequent call re-arming from an already-tripped count.
            st.consecutive_transport_failures = 0;
            tracing::warn!(
                probe_secs = self.probe_after.as_secs(),
                "inference endpoint failed repeatedly; holding background calls"
            );
        }
    }
}

pub struct InteractiveLease {
    gate: Arc<InferenceGate>,
}

impl Drop for InteractiveLease {
    fn drop(&mut self) {
        {
            let mut st = self.gate.state.lock().expect("gate state");
            st.interactive = st.interactive.saturating_sub(1);
            st.last_finished = Some(Instant::now());
        }
        self.gate.resumed.notify_waiters();
    }
}
```

- [ ] **Step 4: Register the module**

In `src/infer/mod.rs`, add after `pub mod facts;`:

```rust
pub mod gate;
```

- [ ] **Step 5: Add the configuration**

In `src/config.rs`, move `cooldown_secs` out of `SynthesizeRole` into a top-level section. Add:

```rust
/// Pacing for every inference call, not just synthesis. The roles share one
/// GPU, so a per-role gap could not bound total load.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PacingConfig {
    /// Minimum seconds between the end of one background call and the start of
    /// the next. Zero disables pacing. `ask` ignores it.
    #[serde(default)]
    pub cooldown_secs: u64,
    #[serde(default = "default_breaker_after")]
    pub breaker_after: usize,
    #[serde(default = "default_breaker_probe_secs")]
    pub breaker_probe_secs: u64,
}

fn default_breaker_after() -> usize {
    3
}

fn default_breaker_probe_secs() -> u64 {
    60
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            cooldown_secs: 0,
            breaker_after: default_breaker_after(),
            breaker_probe_secs: default_breaker_probe_secs(),
        }
    }
}
```

Add `#[serde(default)] pub pacing: PacingConfig,` to `Config`. Keep `SynthesizeRole::cooldown_secs` and `Synthesizer::cooldown()` for now — Task 5 removes them, and removing them here would break `synthesize.rs` before its replacement exists.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib infer::gate && cargo clippy --all-targets && cargo fmt --check`
Expected: 7 passed; clippy silent.

- [ ] **Step 7: Commit**

```bash
git add src/infer/gate.rs src/infer/mod.rs src/config.rs
git commit -m "feat: one pacer in front of every inference call"
```

---

### Task 2: `seq` and round-robin claiming

**Files:**
- Modify: `src/store/schema.sql:113-140`
- Modify: `src/store/jobs.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `Store::enqueue_seq(stage: Stage, target_kind: &str, target_id: &str, seq: i64) -> Result<()>`. `enqueue` keeps its signature and delegates with `seq = 0`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/store/jobs.rs`:

```rust
#[tokio::test]
async fn units_of_two_documents_interleave_rather_than_queueing_behind_each_other() {
    // A thirty-four window document takes thirty-four consecutive row ids. Under
    // id ordering a capture made during that ingest waits for every one of them.
    let s = Store::memory().await.unwrap();
    for i in 0..3 {
        s.enqueue_seq(Stage::SegmentWindow, "segment", &format!("doc-a#{i}"), i)
            .await
            .unwrap();
    }
    for i in 0..3 {
        s.enqueue_seq(Stage::SegmentWindow, "segment", &format!("doc-b#{i}"), i)
            .await
            .unwrap();
    }

    let mut order = Vec::new();
    while let Some(j) = s.claim_job().await.unwrap() {
        order.push(j.target_id);
        s.complete_job(j.id).await.unwrap();
    }
    assert_eq!(
        order,
        vec!["doc-a#0", "doc-b#0", "doc-a#1", "doc-b#1", "doc-a#2", "doc-b#2"],
        "the second document waited for the whole of the first"
    );
}

#[tokio::test]
async fn attempts_still_outrank_seq() {
    // The fairness fix must survive the interleaving one: a unit that keeps
    // failing sinks below fresher work whatever its position in a document.
    let s = Store::memory().await.unwrap();
    s.enqueue_seq(Stage::SegmentWindow, "segment", "sore#0", 0)
        .await
        .unwrap();
    let j = s.claim_job().await.unwrap().unwrap();
    s.fail_job(j.id, 0, "malformed llm output").await.unwrap();

    s.enqueue_seq(Stage::SegmentWindow, "segment", "fresh#9", 9)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET run_after = 0")
        .execute(&s.pool)
        .await
        .unwrap();

    let next = s.claim_job().await.unwrap().unwrap();
    assert_eq!(next.target_id, "fresh#9", "a failing unit kept the front");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib store::jobs`
Expected: FAIL to compile — no `Stage::SegmentWindow`, no `enqueue_seq`.

- [ ] **Step 3: Add the schema column and index**

In `src/store/schema.sql`, add to the `jobs` table body, after `created_at`:

```sql
  -- Position within the batch of units armed together: the window index, the
  -- judge pair's index, the embed batch number. Zero for singletons. Claiming
  -- orders by it so every document's first window runs before any document's
  -- second, which is what stops a large ingest from starving a small one.
  seq         INTEGER NOT NULL DEFAULT 0,
```

Replace the claim index. Note `run_after` sits **last** on purpose: an inequality ends an index's usable ordering, so leading with it would force a temp B-tree on every poll.

```sql
DROP INDEX IF EXISTS idx_jobs_claim;
CREATE INDEX IF NOT EXISTS idx_jobs_claim2  ON jobs(state, attempts, seq, id, run_after);
```

- [ ] **Step 4: Add the stage and the enqueue variant**

In `src/store/jobs.rs`, extend `Stage` with `SegmentWindow`, `Title` and `Judge`, wiring `as_str`/`parse` to `"segment_window"`, `"title"`, `"judge"`.

Add:

```rust
/// Arm a unit at a given position within its batch. `enqueue` is this with
/// `seq = 0`, which is right for singletons and wrong for the thirty-four
/// windows of one document.
pub async fn enqueue_seq(
    &self,
    stage: Stage,
    target_kind: &str,
    target_id: &str,
    seq: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO jobs (stage, target_kind, target_id, state, attempts, run_after, created_at, seq)
         VALUES (?, ?, ?, 'pending', 0, 0, ?, ?)
         ON CONFLICT(stage, target_id) DO UPDATE SET
           state = 'pending', attempts = 0, run_after = 0, last_error = NULL,
           claimed_at = NULL, created_at = excluded.created_at, seq = excluded.seq",
    )
    .bind(stage.as_str())
    .bind(target_kind)
    .bind(target_id)
    .bind(now())
    .bind(seq)
    .execute(&self.pool)
    .await?;
    Ok(())
}
```

Rewrite `enqueue` to delegate: `self.enqueue_seq(stage, target_kind, target_id, 0).await`.

In `claim_job`, change the ordering clause to `ORDER BY attempts, seq, id`. Add `seq = excluded.seq` is **not** wanted in `enqueue_after` — a re-armed unit keeps its position.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib store::jobs && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS, including the pre-existing claim tests.

- [ ] **Step 6: Commit**

```bash
git add src/store/schema.sql src/store/jobs.rs
git commit -m "feat: units of different documents interleave in the queue"
```

---

### Task 3: The `SegmentWindow` handler

**Files:**
- Create: `src/jobs/window.rs`
- Modify: `src/jobs/mod.rs`

**Interfaces:**
- Consumes: `Stage::SegmentWindow` (Task 2).
- Produces: `window::unit_target(corpus_id: &str, idx: i64) -> String` (`"{corpus_id}#{idx}"`); `window::parse_target(t: &str) -> Option<(&str, i64)>`; `async fn window::run(core: &Core, target: &str) -> Result<()>`.

Nothing arms these units yet, so the existing pipeline keeps working and this task lands green. Task 4 flips the pipeline over.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::segments::SegmentState;

    #[test]
    fn a_unit_target_round_trips() {
        let t = unit_target("019ff75a-61b1-7703-aea9-f2a3ae9a0ddd", 17);
        assert_eq!(t, "019ff75a-61b1-7703-aea9-f2a3ae9a0ddd#17");
        assert_eq!(
            parse_target(&t),
            Some(("019ff75a-61b1-7703-aea9-f2a3ae9a0ddd", 17))
        );
        assert_eq!(parse_target("no-hash"), None);
        assert_eq!(parse_target("bad#notanumber"), None);
    }

    #[tokio::test]
    async fn a_unit_segments_exactly_its_own_window() {
        let core = test_core().await;
        let body = (0..400)
            .map(|i| format!("paragraph number {i} with some filler text"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = core.ingest(&body, "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        run(&core, &unit_target(&out.id, 0)).await.unwrap();

        let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
        assert_eq!(windows[0].state, SegmentState::Done);
        assert!(
            windows[1..].iter().all(|w| w.state == SegmentState::Pending),
            "a unit segmented a window that was not its own"
        );
    }

    #[tokio::test]
    async fn a_unit_whose_window_no_longer_exists_is_not_found() {
        // Re-segmenting can shorten a document. The stale unit must be dropped
        // by run_one's NotFound path rather than retried for six hours.
        let core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

        let err = run(&core, &unit_target(&out.id, 99)).await.unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound));
    }

    #[tokio::test]
    async fn an_unreadable_reply_fails_only_this_window() {
        let mut core = test_core().await;
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        core.synthesizer =
            std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on("alpha"));

        let err = run(&core, &unit_target(&out.id, 0)).await.unwrap_err();
        assert!(err.retryable(), "the window is still owed a call");
        let w = &core.store.segments_for_corpus(&out.id).await.unwrap()[0];
        assert!(
            w.last_error.as_deref().is_some_and(|e| e.contains("duplicate field")),
            "the window must carry the parser's own complaint"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib jobs::window`
Expected: FAIL to compile — module does not exist.

- [ ] **Step 3: Implement the handler**

Create `src/jobs/window.rs`. Move `resolve_span`, `write_segment_artifacts`, `from_context_only`, `paraphrased` and `flag_unverified` here from `synthesize.rs` unchanged, then add:

```rust
//! One window, one inference call.
//!
//! This is the unit the whole job model is built around: the smallest thing the
//! synthesizer can be asked to do. Below it there is nothing — one call returns
//! every artifact a window yields, eight on average — and above it was a job
//! covering a whole document, which is what gave thirty-four windows a shared
//! attempt budget and cost one of them twelve rounds of a six-hour backoff.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::segments::SegmentState;

pub fn unit_target(corpus_id: &str, idx: i64) -> String {
    format!("{corpus_id}#{idx}")
}

pub fn parse_target(target: &str) -> Option<(&str, i64)> {
    let (corpus_id, idx) = target.rsplit_once('#')?;
    Some((corpus_id, idx.parse().ok()?))
}

pub async fn run(core: &Core, target: &str) -> Result<()> {
    let (corpus_id, idx) = parse_target(target).ok_or(Error::NotFound)?;
    let w = core
        .store
        .segments_for_corpus(corpus_id)
        .await?
        .into_iter()
        .find(|s| s.idx == idx)
        // The window was re-split out of existence. `run_one` drops the job
        // rather than retrying something that can never come back.
        .ok_or(Error::NotFound)?;

    if w.state == SegmentState::Done {
        return Ok(());
    }

    let all = core.store.segments_for_corpus(corpus_id).await?;
    let all_texts: Vec<&str> = all.iter().map(|s| s.text.as_str()).collect();
    let ctx = crate::infer::context::WindowContext::build(
        &all_texts,
        idx as usize,
        core.synthesizer.budget().context,
        &core.counter,
    );
    let text = w.text.clone();

    core.gate.background().await;
    let first = core
        .synthesizer
        .segment(crate::infer::SegmentInput { core: &text, context: &ctx })
        .await;
    match &first {
        Ok(_) => core.gate.call_succeeded(),
        Err(e) => core.gate.call_failed(e),
    }
    let mut chunks = match first {
        Ok(c) => c,
        Err(e) => {
            let reason = e.to_string();
            tracing::warn!(
                corpus_id,
                window = idx,
                lines = format!("{}-{}", w.start_line, w.end_line),
                reason,
                "window could not be segmented"
            );
            core.store
                .set_segment_state(corpus_id, idx, SegmentState::Failed, Some(&reason))
                .await?;
            settle(core, corpus_id).await?;
            return Err(e);
        }
    };

    if paraphrased(&chunks, &text) {
        tracing::warn!(corpus_id, window = idx, "literals missing; re-segmenting once");
        core.gate.background().await;
        let second = core
            .synthesizer
            .segment(crate::infer::SegmentInput { core: &text, context: &ctx })
            .await;
        match second {
            Ok(c) => {
                core.gate.call_succeeded();
                chunks = c;
            }
            // The first reply parsed; it merely paraphrased. Keeping it and
            // letting `flag_unverified` mark what went missing beats losing a
            // window we can already read.
            Err(e) => {
                core.gate.call_failed(&e);
                tracing::warn!(corpus_id, window = idx, error = %e,
                    "the re-segmentation failed; keeping the first reply");
            }
        }
    }

    if !ctx.is_empty() {
        let before = chunks.len();
        chunks.retain(|c| !from_context_only(&c.text, &text, &ctx));
        let dropped = before - chunks.len();
        if dropped > 0 {
            tracing::info!(corpus_id, window = idx, dropped,
                "artifacts drawn from context blocks were dropped");
        }
    }

    let body: String = text
        .lines()
        .skip(w.carry_lines as usize)
        .collect::<Vec<_>>()
        .join("\n");
    for c in &mut chunks {
        c.corpus_lines = Some(resolve_span(&c.text, &body, &w, c.corpus_lines));
    }

    let written = write_segment_artifacts(core, corpus_id, idx, proposed_to_new(idx, chunks)).await?;
    flag_unverified(core, &written, &text).await?;
    core.store
        .set_segment_state(corpus_id, idx, SegmentState::Done, None)
        .await?;

    settle(core, corpus_id).await
}
```

Leave `settle` as a stub that Task 4 fills in — it must exist for this to compile:

```rust
/// Filled in by Task 4.
async fn settle(_core: &Core, _corpus_id: &str) -> Result<()> {
    Ok(())
}
```

Add `pub mod window;` to `src/jobs/mod.rs` and route the stage:

```rust
(Stage::SegmentWindow, _) => window::run(core, &job.target_id).await,
```

Add `pub gate: Arc<InferenceGate>` to `Core` in `src/core/mod.rs`, built in `from_config` from `cfg.pacing`, and in `test_support` as `Arc::new(InferenceGate::new(Duration::ZERO))`.

Add `pub async fn plan(core: &Core, corpus_id: &str) -> Result<()>` to `synthesize.rs` as the split-and-upsert half of the current `run` — everything before the `for w in ...` loop. Task 4 makes it arm units.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS. The old `synthesize::run` still drives the pipeline.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/window.rs src/jobs/mod.rs src/jobs/synthesize.rs src/core/mod.rs
git commit -m "feat: a window is a schedulable unit"
```

---

### Task 4: Planning, settling, and flipping the pipeline

**Files:**
- Modify: `src/jobs/synthesize.rs`
- Modify: `src/jobs/window.rs`

**Interfaces:**
- Consumes: `window::unit_target`, `window::run`, `Store::enqueue_seq`.
- Produces: `synthesize::plan` arms one `SegmentWindow` per pending window and makes **no** inference call. `window::settle` is real.

- [ ] **Step 1: Write the failing tests**

In `src/jobs/synthesize.rs` tests:

```rust
#[tokio::test]
async fn planning_makes_no_inference_call_and_arms_one_unit_per_window() {
    use crate::infer::fake::RecordingSynthesizer;
    let mut core = test_core().await;
    let rec = std::sync::Arc::new(RecordingSynthesizer::new(context_budget(30, 20)));
    core.synthesizer = rec.clone();
    let body = multi_segment_body();
    let out = core.ingest(&body, "web", None).await.unwrap();

    plan(&core, &out.id).await.unwrap();

    assert_eq!(rec.seen.lock().unwrap().len(), 0, "planning called the model");
    let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
    let armed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'",
    )
    .fetch_one(&core.store.pool)
    .await
    .unwrap();
    assert_eq!(armed as usize, windows);
}

#[tokio::test]
async fn a_poisoned_window_does_not_stop_another_document() {
    // The 2026-08-12 incident as a test. Document A holds a window whose reply
    // never parses; document B must still reach ready.
    let mut core = test_core().await;
    let a = core
        .ingest(&format!("STOPHERE poison\n\n{}", multi_segment_body()), "web", None)
        .await
        .unwrap();
    let b = core.ingest("bravo one\n\nbravo two", "web", None).await.unwrap();
    core.synthesizer =
        std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"));

    for _ in 0..200 {
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&core.store.pool)
            .await
            .unwrap();
        if !crate::jobs::run_one(&core).await.unwrap_or(false) {
            break;
        }
    }

    assert_eq!(
        core.store.get_corpus(&b.id).await.unwrap().status,
        CorpusStatus::Ready,
        "the healthy document waited on the poisoned one"
    );
    assert_eq!(
        core.store.get_corpus(&a.id).await.unwrap().status,
        CorpusStatus::Partial,
        "a document with one refused window settles partial, not failed"
    );
}

#[tokio::test]
async fn a_corpus_settles_around_a_window_that_will_not_resolve() {
    // Per-unit budgets could hang a corpus forever: if no unit terminates, the
    // settle never fires and thirty-three good windows never become searchable.
    let mut core = test_core().await;
    let body = format!("STOPHERE poison\n\n{}", multi_segment_body());
    let out = core.ingest(&body, "web", None).await.unwrap();
    core.synthesizer =
        std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"));

    for _ in 0..200 {
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&core.store.pool)
            .await
            .unwrap();
        if !crate::jobs::run_one(&core).await.unwrap_or(false) {
            break;
        }
    }

    assert_eq!(
        core.store.get_corpus(&out.id).await.unwrap().status,
        CorpusStatus::Partial
    );
    assert!(
        !core.store.artifacts_for_corpus(&out.id).await.unwrap().is_empty(),
        "the good windows' artifacts were never written"
    );
    let embed_armed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE stage = 'embed' AND target_id = ?",
    )
    .bind(&out.id)
    .fetch_one(&core.store.pool)
    .await
    .unwrap();
    assert_eq!(embed_armed, 1, "the good artifacts were never queued to embed");
}

#[tokio::test]
async fn a_recovered_window_settles_the_corpus_again() {
    let mut core = test_core().await;
    let body = format!("STOPHERE poison\n\n{}", multi_segment_body());
    let out = core.ingest(&body, "web", None).await.unwrap();
    core.synthesizer =
        std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::unparsable_on("STOPHERE"));
    for _ in 0..200 {
        sqlx::query("UPDATE jobs SET run_after = 0").execute(&core.store.pool).await.unwrap();
        if !crate::jobs::run_one(&core).await.unwrap_or(false) { break; }
    }

    core.synthesizer = std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::default());
    for _ in 0..200 {
        sqlx::query("UPDATE jobs SET run_after = 0").execute(&core.store.pool).await.unwrap();
        if !crate::jobs::run_one(&core).await.unwrap_or(false) { break; }
    }

    let windows = core.store.segments_for_corpus(&out.id).await.unwrap();
    assert!(windows.iter().all(|w| w.state == SegmentState::Done));
    assert_eq!(
        core.store.get_corpus(&out.id).await.unwrap().status,
        CorpusStatus::Ready
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::synthesize`
Expected: FAIL — planning arms nothing; the poisoned document blocks the other.

- [ ] **Step 3: Make `plan` arm units**

At the end of `synthesize::plan`, after `upsert_segments`:

```rust
    // One unit per window that has not resolved. `seq` is the window index, so
    // this document's window 0 is claimed before any document's window 1.
    for w in core.store.pending_segments(corpus_id).await? {
        core.store
            .enqueue_seq(
                Stage::SegmentWindow,
                "segment",
                &crate::jobs::window::unit_target(corpus_id, w.idx),
                w.idx,
            )
            .await?;
    }
    Ok(())
```

Delete the window loop and the old `run`. Point the `Synthesize | Enrich` dispatch arm in `jobs/mod.rs` at `synthesize::plan`.

- [ ] **Step 4: Implement `settle`**

Replace the stub in `src/jobs/window.rs`:

```rust
/// Everything that can only be decided once every window has resolved.
///
/// "Resolved" has to include a window that has spent its attempts, or a corpus
/// would hang forever on one the model will not read: engram never abandons
/// work, so that unit stays queued at the backoff ceiling and would otherwise
/// hold thirty-three good windows out of the index indefinitely. The corpus
/// settles around it and reports `partial`. If it later succeeds, this runs
/// again — every step here is idempotent for exactly that reason.
async fn settle(core: &Core, corpus_id: &str) -> Result<()> {
    let windows = core.store.segments_for_corpus(corpus_id).await?;
    let unresolved = windows.iter().any(|w| {
        w.state == SegmentState::Pending
            || (w.state == SegmentState::Failed && attempts_for(core, corpus_id, w.idx) < MAX_ATTEMPTS)
    });
    if unresolved {
        return Ok(());
    }
    crate::jobs::synthesize::finish(core, corpus_id).await
}
```

`attempts_for` reads the unit's own job row, since `jobs.attempts` is now the single counter:

```rust
async fn attempts_for(core: &Core, corpus_id: &str, idx: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT attempts FROM jobs WHERE stage = 'segment_window' AND target_id = ?",
    )
    .bind(unit_target(corpus_id, idx))
    .fetch_optional(&core.store.pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(MAX_ATTEMPTS)
}
```

Make the `unresolved` closure `async`-friendly by collecting attempts first:

```rust
    let mut unresolved = false;
    for w in &windows {
        unresolved |= match w.state {
            SegmentState::Pending => true,
            SegmentState::Failed => attempts_for(core, corpus_id, w.idx).await < MAX_ATTEMPTS,
            SegmentState::Done => false,
        };
    }
```

In `synthesize::finish`, remove the title call (Task 6 makes it a unit) and arm it instead:

```rust
    if src.title_hint.is_none() {
        core.store.enqueue(Stage::Title, "corpus", corpus_id).await?;
    }
```

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS. Some old `synthesize` tests now fail — Task 10 removes them. If any block this task, mark them `#[ignore]` with a `// removed in Task 10` comment rather than deleting them here.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/synthesize.rs src/jobs/window.rs src/jobs/mod.rs
git commit -m "feat: planning arms window units and the corpus settles around them"
```

---

### Task 5: Wire the gate into `ask` and retire the per-role cooldown

**Files:**
- Modify: `src/core/ask.rs`
- Modify: `src/infer/mod.rs`, `src/infer/openai.rs`, `src/infer/fake.rs`, `src/config.rs`

**Interfaces:**
- Consumes: `Core::gate`.
- Produces: `Synthesizer::cooldown()` and `SynthesizeRole::cooldown_secs` are gone.

- [ ] **Step 1: Write the failing test**

In `src/core/ask.rs` tests:

```rust
#[tokio::test(start_paused = true)]
async fn a_question_holds_the_interactive_lane_for_its_whole_answer() {
    use std::time::Duration;
    let core = crate::core::test_support::test_core().await;
    let gate = std::sync::Arc::clone(&core.gate);

    let asking = tokio::spawn({
        let core = core.clone();
        async move {
            core.ask(&AskRequest { question: "what".into(), ..Default::default() }).await
        }
    });
    tokio::time::advance(Duration::from_millis(1)).await;

    let waiter = tokio::spawn(async move { gate.background().await });
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(!waiter.is_finished(), "background work ran during an ask");

    asking.await.unwrap().unwrap();
    waiter.await.unwrap();
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib core::ask`
Expected: FAIL — background work is not held off.

- [ ] **Step 3: Take the lease**

At the top of `Core::ask`, before any inference:

```rust
    // Held for the whole answer, not per call: `ask` makes more than one, and a
    // gap between them is a gap the worker would fill with a window.
    let _lane = self.gate.interactive();
```

- [ ] **Step 4: Remove the per-role cooldown**

Delete `Synthesizer::cooldown()` from the trait in `src/infer/mod.rs` and its implementations in `openai.rs` and `fake.rs` (including `PacedSynthesizer`, whose only purpose was that hook — replace its test with the gate's cooldown test from Task 1). Delete `SynthesizeRole::cooldown_secs` and `cooldown_secs = 0` from `config.example.toml`, adding a `[pacing]` section documenting `cooldown_secs`, `breaker_after`, `breaker_probe_secs`.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/core/ask.rs src/infer src/config.rs config.example.toml
git commit -m "feat: ask holds the interactive lane; cooldown becomes global"
```

---

### Task 6: `Title` as a unit

**Files:**
- Modify: `src/jobs/synthesize.rs`, `src/jobs/mod.rs`

**Interfaces:**
- Produces: `async fn synthesize::run_title(core: &Core, corpus_id: &str) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn naming_a_corpus_is_its_own_unit() {
    let core = test_core().await;
    let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
    plan(&core, &out.id).await.unwrap();
    while crate::jobs::run_one(&core).await.unwrap() {}
    assert_eq!(
        core.store.get_corpus(&out.id).await.unwrap().title_hint.as_deref(),
        Some("Fake title: alpha line")
    );
}

#[tokio::test]
async fn a_corpus_the_model_will_not_name_still_reaches_ready() {
    // A name is decoration. Retrying it forever spends real calls on it, and
    // failing the corpus over it would be worse.
    let mut core = test_core().await;
    let out = core.ingest("alpha line\n\nbravo line", "web", None).await.unwrap();
    plan(&core, &out.id).await.unwrap();
    while crate::jobs::run_one(&core).await.unwrap() {}
    core.synthesizer =
        std::sync::Arc::new(crate::infer::fake::FakeSynthesizer::failing("no title for you"));
    core.store.enqueue(Stage::Title, "corpus", &out.id).await.unwrap();

    for _ in 0..MAX_ATTEMPTS + 2 {
        sqlx::query("UPDATE jobs SET run_after = 0").execute(&core.store.pool).await.unwrap();
        let _ = crate::jobs::run_one(&core).await;
    }

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE stage = 'title' AND state = 'pending'",
    )
    .fetch_one(&core.store.pool)
    .await
    .unwrap();
    assert_eq!(queued, 0, "a cosmetic failure is retried forever");
    assert!(core.store.get_corpus(&out.id).await.unwrap().title_hint.is_none());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::synthesize`
Expected: FAIL — no `title` stage handler.

- [ ] **Step 3: Implement**

```rust
/// Name a document. A failure past the attempt budget closes the job rather
/// than retrying at the ceiling: the corpus keeps the snippet the UI falls back
/// to, and a name is not worth four model calls a day forever.
pub async fn run_title(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.title_hint.is_some() {
        return Ok(());
    }
    let titles: Vec<String> = core
        .store
        .artifacts_for_corpus(corpus_id)
        .await?
        .iter()
        .filter_map(|c| c.title.clone())
        .collect();

    core.gate.background().await;
    match core.synthesizer.title(&src.raw_text, &titles).await {
        Ok(Some(t)) => {
            core.gate.call_succeeded();
            core.store.set_title_hint(corpus_id, &t).await?;
            Ok(())
        }
        Ok(None) => {
            core.gate.call_succeeded();
            Ok(())
        }
        Err(e) => {
            core.gate.call_failed(&e);
            Err(e)
        }
    }
}
```

In `jobs/mod.rs`, dispatch `(Stage::Title, _) => synthesize::run_title(core, &job.target_id).await`, and in the retryable arm add the give-up case:

```rust
    // A name is decoration; the corpus already has its fallback.
    (Stage::Title, _) if exhausted => {
        tracing::warn!(error = %e, "could not name this corpus; leaving it unnamed");
        core.store.complete_job(job.id).await?;
    }
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 5: Commit**

```bash
git add src/jobs/synthesize.rs src/jobs/mod.rs
git commit -m "feat: naming a document is its own unit"
```

---

### Task 7: One embed batch per unit

**Files:**
- Modify: `src/jobs/embed.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_large_corpus_embeds_one_batch_per_claim() {
    let core = crate::core::test_support::test_core().await;
    let body = (0..70)
        .map(|i| format!("paragraph {i} with filler"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let out = core.ingest(&body, "web", None).await.unwrap();
    crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
    while crate::jobs::run_one(&core).await.unwrap() {}

    let chunks = core.store.artifacts_for_corpus(&out.id).await.unwrap().len();
    assert!(chunks > BATCH, "the fixture must exceed one batch");
    assert!(
        core.store.pending_artifacts_for_corpus(&out.id).await.unwrap().is_empty(),
        "the re-arm did not drain the corpus"
    );
}

#[tokio::test]
async fn one_run_embeds_at_most_one_batch() {
    let core = crate::core::test_support::test_core().await;
    let body = (0..70).map(|i| format!("p {i}")).collect::<Vec<_>>().join("\n\n");
    let out = core.ingest(&body, "web", None).await.unwrap();
    crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
    while let Some(j) = core.store.claim_job().await.unwrap() {
        if j.stage == crate::store::jobs::Stage::Embed { break; }
        let _ = crate::jobs::run_one(&core).await;
        core.store.complete_job(j.id).await.unwrap();
    }
    let before = core.store.pending_artifacts_for_corpus(&out.id).await.unwrap().len();
    run_corpus(&core, &out.id).await.unwrap();
    let after = core.store.pending_artifacts_for_corpus(&out.id).await.unwrap().len();
    assert_eq!(before - after, BATCH, "a run embedded more than one batch");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib jobs::embed`
Expected: FAIL — `run_corpus` drains every batch in one call.

- [ ] **Step 3: Implement**

In `run_corpus_with_limit`, replace the `for (chunks, texts) in batch.chunks(BATCH)...` loop with a single batch plus a re-arm:

```rust
    let (chunks, texts) = match (batch.get(..BATCH.min(batch.len())), texts.get(..BATCH.min(texts.len()))) {
        (Some(c), Some(t)) if !c.is_empty() => (c, t.to_vec()),
        _ => return settle_corpus(core, corpus_id).await,
    };

    core.gate.background().await;
    match embed_batch(core, chunks, texts).await {
        Ok(()) => core.gate.call_succeeded(),
        Err(e) if input_too_large(&e) => {
            core.gate.call_failed(&e);
            tracing::warn!(corpus_id, error = %e, "batch held a chunk the endpoint will not take; isolating");
            return split_into_artifact_jobs(core, corpus_id).await;
        }
        Err(e) => {
            core.gate.call_failed(&e);
            return Err(e);
        }
    }

    // More to do: come back as a fresh unit rather than draining the corpus in
    // one job, so a large document cannot monopolise the worker and each batch
    // is paced and preemptible. `seq` climbs so later batches sink below other
    // documents' first ones.
    if !core.store.pending_artifacts_for_corpus(corpus_id).await?.is_empty() {
        let next_seq = core.store.job_seq(Stage::Embed, corpus_id).await?.unwrap_or(0) + 1;
        core.store
            .enqueue_seq(Stage::Embed, "corpus", corpus_id, next_seq)
            .await?;
        return Ok(());
    }
    settle_corpus(core, corpus_id).await
```

Add to `src/store/jobs.rs`:

```rust
/// The `seq` a job currently carries, so a re-arming unit can climb rather than
/// re-entering at the front.
pub async fn job_seq(&self, stage: Stage, target_id: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT seq FROM jobs WHERE stage = ? AND target_id = ?",
    )
    .bind(stage.as_str())
    .bind(target_id)
    .fetch_optional(&self.pool)
    .await?)
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 5: Commit**

```bash
git add src/jobs/embed.rs src/store/jobs.rs
git commit -m "feat: one embed batch per unit, re-arming while chunks remain"
```

---

### Task 8: `Judge` as a unit

**Files:**
- Create: `src/jobs/judge.rs`
- Modify: `src/jobs/consolidate.rs`, `src/jobs/mod.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn the_sweep_makes_no_inference_call_and_arms_one_unit_per_pair() {
    // Twenty judge calls in one job was the second-worst blocker in the system.
    let core = crate::core::test_support::test_core_with_pairs().await;
    let before = core.completer_calls();

    crate::jobs::consolidate::run(&core).await.unwrap();

    assert_eq!(core.completer_calls(), before, "the sweep called the model");
    let armed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE stage = 'judge' AND state = 'pending'",
    )
    .fetch_one(&core.store.pool)
    .await
    .unwrap();
    assert!(armed > 0, "no judge units were armed");
}
```

`test_core_with_pairs` does not exist yet. Add it to `test_support` in `src/core/mod.rs`. `record_pair(a, b, score)` writes a `pending` row directly, so the helper does not need a similarity sweep to produce one:

```rust
/// A core holding two artifacts and one pending pair between them, plus a
/// counting completer so a test can assert the sweep made no call.
pub async fn test_core_with_pairs() -> (Core, Arc<ScriptedCompleter>) {
    let completer = Arc::new(ScriptedCompleter::new(vec![
        r#"{"contradicts":false}"#.to_string(),
    ]));
    let mut core = build(Arc::new(FakeSynthesizer::default()), None).await;
    core.completer = completer.clone();

    let out = core
        .ingest("alpha paragraph here\n\nbravo paragraph here", "web", None)
        .await
        .unwrap();
    crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
    while crate::jobs::run_one(&core).await.unwrap() {}

    let artifacts = core.store.artifacts_for_corpus(&out.id).await.unwrap();
    assert!(artifacts.len() >= 2, "the fixture needs two artifacts to pair");
    core.store
        .record_pair(&artifacts[0].id, &artifacts[1].id, 0.9)
        .await
        .unwrap();

    (core, completer)
}
```

`ScriptedCompleter::calls()` already exists (`src/infer/fake.rs:514`), so the test reads `completer.calls()` rather than a new `Core` method. Adjust the test's first and third lines to:

```rust
    let (core, completer) = crate::core::test_support::test_core_with_pairs().await;
    let before = completer.calls();
    // ...
    assert_eq!(completer.calls(), before, "the sweep called the model");
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib jobs::consolidate`
Expected: FAIL — the sweep judges inline.

- [ ] **Step 3: Implement**

Move the body of `judge_pending`'s loop into `src/jobs/judge.rs`:

```rust
//! One pair, one call.
//!
//! The sweep used to make up to `max_judgements` of these in a single job, so a
//! consolidation run blocked every capture behind it for as long as twenty
//! model calls took. The sweep now arms them and the queue paces them.

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let Some(p) = core.store.pair(id).await? else {
        return Err(Error::NotFound);
    };
    let (Ok(a), Ok(b)) = (
        core.store.get_artifact(&p.a_id).await,
        core.store.get_artifact(&p.b_id).await,
    ) else {
        // One side was deleted or superseded while the unit waited.
        core.store.record_judge_attempt(id).await?;
        return Ok(());
    };

    core.gate.background().await;
    let out = core
        .completer
        .complete(
            crate::infer::prompt::JUDGE_SYSTEM,
            &crate::infer::prompt::judge_prompt(
                (a.title.as_deref().unwrap_or(""), &a.text),
                (b.title.as_deref().unwrap_or(""), &b.text),
            ),
        )
        .await;
    match &out {
        Ok(_) => core.gate.call_succeeded(),
        Err(e) => core.gate.call_failed(e),
    }
    let (contradicts, detail, obsolete) = crate::infer::prompt::parse_judgement(&out?)?;
    core.store.record_judge_attempt(id).await?;
    apply_judgement(core, &p, contradicts, detail, obsolete).await
}
```

`apply_judgement` is the existing post-parse half of `judge_pending`, moved verbatim. Add `Store::pair(id) -> Result<Option<ArtifactPair>>` alongside `pairs_to_judge`.

In `consolidate.rs`, replace the `judge_pending` call with:

```rust
    if cfg.judge {
        for (n, p) in core.store.pairs_to_judge(200).await?.into_iter().enumerate() {
            if n >= core.consolidate.max_judgements {
                break;
            }
            core.store
                .enqueue_seq(Stage::Judge, "pair", &p.id.to_string(), n as i64)
                .await?;
        }
    }
```

`seq` is the pair's index in the sweep, so twenty judge units do not all sit at `seq = 0` and jump the whole window queue.

Dispatch `(Stage::Judge, _) => judge::run(core, &job.target_id).await`, and give it the same give-up arm as `Title`: past `MAX_ATTEMPTS` complete the job and leave the pair pending for a later sweep.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 5: Commit**

```bash
git add src/jobs/judge.rs src/jobs/consolidate.rs src/jobs/mod.rs src/store/pairs.rs src/core/mod.rs
git commit -m "feat: a judgement is its own unit"
```

---

### Task 9: Reconcile and the deployment path

**Files:**
- Modify: `src/jobs/reconcile.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn an_old_corpus_level_job_becomes_per_window_units() {
    // The deployment path: a database written before this change holds one
    // Synthesize row per unfinished corpus and no window units at all.
    let core = crate::core::test_support::test_core().await;
    let body = (0..400)
        .map(|i| format!("paragraph {i} with filler text"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let out = core.ingest(&body, "web", None).await.unwrap();
    crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();

    // Wind the clock back to the old shape.
    sqlx::query("DELETE FROM jobs WHERE stage = 'segment_window'")
        .execute(&core.store.pool)
        .await
        .unwrap();
    core.store
        .enqueue(crate::store::jobs::Stage::Synthesize, "corpus", &out.id)
        .await
        .unwrap();

    run(&core).await.unwrap();

    let armed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE stage = 'segment_window' AND state = 'pending'",
    )
    .fetch_one(&core.store.pool)
    .await
    .unwrap();
    let windows = core.store.segments_for_corpus(&out.id).await.unwrap().len();
    assert_eq!(armed as usize, windows, "the old job did not become units");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib jobs::reconcile`
Expected: FAIL — reconcile arms a corpus-level `Synthesize` job only.

- [ ] **Step 3: Implement**

In `reconcile::run`, replace the unfinished-segments branch:

```rust
            let segments = core.store.segments_for_corpus(&c.id).await?;
            if segments.is_empty() {
                // Never planned. The planning stage splits and arms the units.
                core.store.enqueue(Stage::Synthesize, "corpus", &c.id).await?;
                armed += 1;
                continue;
            }
            let unresolved: Vec<_> = segments
                .iter()
                .filter(|w| w.state != SegmentState::Done)
                .collect();
            if !unresolved.is_empty() {
                // Windows exist but their units do not — either a database from
                // before units existed, or a process killed between two writes.
                for w in unresolved {
                    core.store
                        .enqueue_seq(
                            Stage::SegmentWindow,
                            "segment",
                            &crate::jobs::window::unit_target(&c.id, w.idx),
                            w.idx,
                        )
                        .await?;
                    armed += 1;
                }
                continue;
            }
```

`enqueue_seq` is idempotent per `(stage, target_id)`, so a sweep over a healthy base still costs one query per hundred corpora and changes nothing.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib && cargo clippy --all-targets && cargo fmt --check`

- [ ] **Step 5: Commit**

```bash
git add src/jobs/reconcile.rs
git commit -m "feat: reconcile arms window units, which is the upgrade path"
```

---

### Task 10: Delete what the granularity change made dead

**Files:**
- Modify: `src/jobs/synthesize.rs`, `src/jobs/mod.rs`, `src/store/segments.rs`, `src/store/schema.sql`

- [ ] **Step 1: Delete the code**

- `fail_pending_segments` and its call site in `jobs/mod.rs`, plus the `(Stage::Synthesize, _) if exhausted` arm. Planning makes no inference call, so it cannot exhaust anything.
- `REFUSALS_BEFORE_GIVING_UP_ON_THE_PASS` and the `in_a_row` counter.
- `Store::bump_segment_attempts` and every caller. `jobs.attempts` is the single counter.

In `schema.sql`, leave `segments.attempts` in place — `migrate()` cannot drop a column — and comment it:

```sql
  -- Dead since 2026-08-13: `jobs.attempts` is the per-window counter now that a
  -- window is its own unit. Left in place because `migrate` cannot drop a
  -- column; remove when the database is next recreated.
  attempts   INTEGER NOT NULL DEFAULT 0,
```

- [ ] **Step 2: Delete or rewrite the obsolete tests**

Delete, with their premises: `a_window_the_model_refuses_is_marked_failed_not_split`, `a_burst_of_endpoint_failures_does_not_condemn_untried_windows`, `a_source_with_untried_windows_still_has_a_job_after_a_failure`, `a_segment_the_endpoint_refused_is_queued_again`, `a_model_refusing_everything_costs_a_handful_of_calls_not_one_per_window`, `a_window_the_parser_chokes_on_does_not_hold_up_the_rest_of_the_document`, `a_cooldown_paces_the_windows_it_segments`. Each asserts behaviour that only exists with a shared attempt budget or a per-role cooldown; Tasks 1, 4 and 6 cover the same ground at the new granularity.

Un-ignore anything marked `// removed in Task 10` in Task 4 and delete it.

- [ ] **Step 3: Run the whole suite**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS, no warnings.

- [ ] **Step 4: Verify the schema applies to the real database**

```bash
cp engram.db /tmp/upgrade-check.db
sqlite3 /tmp/upgrade-check.db < <(sed -n '/── Jobs/,/idx_jobs_created/p' src/store/schema.sql)
sqlite3 /tmp/upgrade-check.db "EXPLAIN QUERY PLAN
  SELECT id FROM jobs WHERE state='pending' AND run_after <= 9999999999
  ORDER BY attempts, seq, id LIMIT 1;"
```

Expected: `SEARCH jobs USING COVERING INDEX idx_jobs_claim2`, with **no** `USE TEMP B-TREE FOR ORDER BY`. A temp B-tree means the index column order is wrong — `run_after` must be last.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: remove what a shared attempt budget needed"
```

---

## Self-Review

**Spec coverage.** §3 unit model → Tasks 3, 4, 6, 7, 8. §4 gate, breaker, claim ordering, reconcile → Tasks 1, 2, 5, 9. §5 budgets and settle → Task 4 (settle), Tasks 6 and 8 (Title/Judge give-up arms). §6 deletions → Task 10. §7 schema and deployment → Tasks 2, 9, 10. §8 affinity → no task; it is a decision not to build something. §9 tests → all sixteen are placed: gate 1–5 in Task 1, 6–8 in Task 4, 9 in Task 2, 10–11 in Task 4, 12 in Task 3, 13 in Task 6, 14 in Task 8, 15 in Task 7, 16 in Task 9. §10 → Task 0. §11 out of scope, no tasks.

**Type consistency.** `unit_target`/`parse_target` defined in Task 3, used in Tasks 4 and 9. `enqueue_seq` defined in Task 2, used in Tasks 4, 7, 8, 9. `job_seq` defined in Task 7, used only there. `Core::gate` added in Task 3, used in Tasks 3, 5, 6, 7, 8. `synthesize::plan` introduced in Task 3, completed in Task 4, used in Tasks 6, 7, 9. `settle` stubbed in Task 3, implemented in Task 4.

**Known rough edge, flagged rather than hidden.** Task 4 may leave a handful of old tests temporarily `#[ignore]`d until Task 10. That is deliberate: it lets the pipeline flip land green in one commit instead of dragging test deletion into it. Every such marker carries a `// removed in Task 10` comment, and Task 10 step 2 sweeps them.

**Risk worth naming before execution.** Task 4 is the one that flips the pipeline, and it is the largest. If it does not land cleanly, the failure mode is a half-migrated system where planning arms units but the old loop still runs — so it must be reviewed as a whole and not split further at execution time. Tasks 1, 2, 3 are all additive and safe to land independently; Task 4 is the commit to be careful about.
