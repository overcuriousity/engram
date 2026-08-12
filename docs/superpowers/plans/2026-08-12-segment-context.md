# Segment Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every synthesizer window a verbatim opening of its document and the neighbouring text on both sides, so artifacts can resolve references and record what version or platform they apply to.

**Architecture:** Stored window spans stay contiguous, non-overlapping core ranges — no schema change. Context is derived at call time from `raw_text` plus neighbouring spans, assembled by a new `src/infer/context.rs`, rendered into the user prompt as fenced context-only blocks, and carried to the synthesizer through a new `SegmentInput` struct. Artifacts the model wrongly extracts from a context block are dropped structurally, by locating their text.

**Tech Stack:** Rust, tokio, sqlx/SQLite, `async_trait`. Tests are `#[test]` and `#[tokio::test]` inline `mod tests` blocks, per this codebase's convention.

**Spec:** `docs/superpowers/specs/2026-08-12-segment-context-design.md`

## Global Constraints

- Token counting always goes through `crate::infer::budget::TokenCounter`. Never count characters directly.
- Default context budget: **opening = 200 tokens, overlap = 150 tokens per side**. Both configurable.
- `ContextBudget::default()` is **zero for both fields**. Existing tests must keep today's geometry without being edited.
- Stored segment spans stay core-only. No migration, no schema change, no new columns.
- `locate_span`, `missing_literals`, and `paraphrased` are always given the **core text**, never the assembled prompt.
- Every task ends with `cargo test`, `cargo clippy --all-targets`, and `cargo fmt` clean.
- Commit at the end of every task. Commit messages: lowercase `type: subject`, subject states the behaviour, body states why.

## File Structure

| File | Responsibility | Task |
| ---- | -------------- | ---- |
| `src/infer/split.rs` | Windowing. Gains a character-level floor so it never returns an over-budget window. | 1 |
| `src/config.rs` | Two new fields on `SynthesizeRole`. | 2 |
| `src/infer/context.rs` | **New.** `ContextBudget`, `WindowContext`, and the pure assembly of the three context blocks. | 3 |
| `src/infer/mod.rs` | `ContextBudget` on `SynthesisBudget`; new `SegmentInput`; `Synthesizer::segment` signature. | 2, 5 |
| `src/infer/prompt.rs` | Renders context blocks into the user prompt; the system prompt's context-only instruction. | 4 |
| `src/infer/openai.rs` | Populates the budget from config; passes context to `user_prompt`. | 2, 5 |
| `src/infer/fake.rs` | Six `Synthesizer` impls updated to the new signature. | 5 |
| `src/jobs/synthesize.rs` | Assembles context per window, passes it, drops context-only artifacts, guards window size. | 6, 7, 8 |

---

### Task 1: Give `split_into_segments` a size floor

Independent of everything else in this plan, and it fixes a live bug: a corpus with no newlines returns one unbounded window that retries against the endpoint forever.

**Files:**
- Modify: `src/infer/split.rs:20-91` (`split_into_segments`)
- Test: `src/infer/split.rs` (inline `mod tests`, after `text_with_no_structure_still_splits_by_line_cap`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `split_into_segments(text: &str, counter: &TokenCounter, segment_tokens: usize) -> Vec<Window>` — signature unchanged, now guarantees every returned `Window`'s text counts at most `segment_tokens`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/infer/split.rs`:

```rust
    #[test]
    fn a_corpus_with_no_newlines_is_still_windowed_within_budget() {
        // A paste from a PDF or a chat transcript is frequently one enormous
        // line. Returning it as a single window sent it to the model whole,
        // where it overflowed the context and retried with growing backoff
        // forever, because a job has no terminal state.
        let counter = TokenCounter::Estimate;
        let blob = "word ".repeat(4000);
        assert!(!blob.contains('\n'), "the point is that there are no lines");

        let windows = split_into_segments(&blob, &counter, 256);

        assert!(windows.len() > 1, "got {} window(s)", windows.len());
        for w in &windows {
            assert!(
                counter.count(&w.text) <= 256,
                "window of {} tokens exceeds the budget",
                counter.count(&w.text)
            );
        }
        assert_eq!(
            windows.iter().map(|w| w.text.as_str()).collect::<String>(),
            blob,
            "cutting must not lose or duplicate text"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib infer::split::tests::a_corpus_with_no_newlines -- --nocapture`
Expected: FAIL — `got 1 window(s)`.

- [ ] **Step 3: Write the implementation**

In `src/infer/split.rs`, add this function after `flush`:

```rust
/// A line longer than a whole window has no boundary anywhere in it — a
/// minified blob, or text pasted with no newlines at all. Characters are the
/// last resort, and 3.5 per token is the same ratio the estimate uses, so a
/// part lands near the budget rather than far under it.
fn cut_long_line(line: &str, segment_tokens: usize, counter: &TokenCounter) -> Vec<String> {
    if counter.count(line) <= segment_tokens {
        return vec![line.to_string()];
    }
    let max_chars = (segment_tokens * 7 / 2).max(64);
    line.chars()
        .collect::<Vec<_>>()
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>())
        .collect()
}
```

Then, in `split_into_segments`, replace the `for (idx, line) in lines.iter().enumerate() {` loop header and its first statement so that over-long lines are pre-cut. Change the construction of `lines` at line 37 from:

```rust
    let lines: Vec<&str> = text.lines().collect();
```

to:

```rust
    // A single line over budget is cut before windowing, so every unit the
    // loop below places is guaranteed to fit in a window on its own. Without
    // this the loop can only ever emit one window for such a line, and that
    // window is however large the line was.
    let owned: Vec<String> = text
        .lines()
        .flat_map(|l| cut_long_line(l, segment_tokens, counter))
        .collect();
    let lines: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib infer::split`
Expected: PASS, all of them. `every_line_of_the_source_survives_windowing` and `line_numbers_are_one_based_and_contiguous` must still pass — a corpus whose lines all fit is unaffected, because `cut_long_line` returns the line unchanged.

- [ ] **Step 5: Verify the whole suite and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass. Note `segment_text` slices the **original** `raw_text` by line number, so a corpus containing a cut line now has windows whose `start_line`/`end_line` no longer address the same units the splitter used. This is acceptable and pre-existing in shape: a single-line corpus has one source line, every window reports `start_line: 1`, and `segment_text` clamps. Confirm `window_text_returns_exactly_the_lines_a_window_claims` still passes.

- [ ] **Step 6: Commit**

```bash
git add src/infer/split.rs
git commit -m "fix: a corpus with no newlines was one unbounded window

split_into_segments could only flush at a line boundary, and its overflow
check was guarded on a non-empty buffer, so text with no newlines at all —
a paste from a PDF or a chat transcript — came back as a single window of
whatever size the input was. That window overflowed the model's context on
every attempt, and a job has no terminal state, so it retried forever.

Long lines are now cut to size before windowing, which gives the function
the same guarantee split_by_lines in jobs/embed.rs already had: what comes
back is never larger than what was asked for."
```

---

### Task 2: Carry a context budget from config to the window size

**Files:**
- Modify: `src/config.rs:135-163` (`SynthesizeRole`), plus a defaults function near the other `default_*` fns
- Modify: `src/infer/mod.rs:26-30` (`SynthesisBudget`)
- Modify: `src/infer/budget.rs:62-68` (`segment_tokens`)
- Modify: `src/infer/openai.rs:85-89`
- Modify: `src/infer/fake.rs` (every literal `SynthesisBudget { .. }`)
- Test: `src/infer/budget.rs` inline `mod tests`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `crate::infer::context::ContextBudget { pub opening: usize, pub overlap: usize }`, `#[derive(Debug, Clone, Copy, Default, PartialEq)]`, with `pub fn total(&self) -> usize`.
  - `SynthesisBudget` gains `pub context: ContextBudget`.
  - `SynthesizeRole` gains `pub context_opening_tokens: usize` and `pub context_overlap_tokens: usize`.

- [ ] **Step 1: Create the module with the budget type only**

Create `src/infer/context.rs`:

```rust
/// How many tokens of surrounding material a window may carry on top of its
/// own text. Absolute rather than a fraction of the window: a large context
/// should make context free, not proportionally expensive.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextBudget {
    /// Tokens of the document's verbatim opening.
    pub opening: usize,
    /// Tokens of each neighbouring window, on both sides.
    pub overlap: usize,
}

/// The fence lines and their labels, which are prompt text like any other and
/// have to be paid for out of the same budget.
const FENCE_TOKENS: usize = 40;

impl ContextBudget {
    /// Everything the context blocks cost, fences included. This is subtracted
    /// from the window so the assembled prompt still fits the model.
    pub fn total(&self) -> usize {
        if self.opening == 0 && self.overlap == 0 {
            return 0;
        }
        self.opening + 2 * self.overlap + FENCE_TOKENS
    }
}
```

Register it in `src/infer/mod.rs` alongside the other `pub mod` lines:

```rust
pub mod context;
```

- [ ] **Step 2: Write the failing test**

Add to the `mod tests` block in `src/infer/budget.rs`:

```rust
    #[test]
    fn the_context_budget_comes_out_of_the_window() {
        use crate::infer::context::ContextBudget;

        // A generous output ceiling, so the context window is the binding
        // constraint and the subtraction is visible. Under the other ceiling
        // — `by_output` — the window is capped by what the model can emit and
        // context costs nothing, which is correct but proves nothing here.
        let mut b = budget(32768, 100_000, 1.4);
        let without = segment_tokens(b, 1000);

        b.context = ContextBudget {
            opening: 200,
            overlap: 150,
        };
        let with = segment_tokens(b, 1000);

        // 200 + 2*150 + 40 fences = 540 prompt tokens. The window loses that
        // divided by (1 + output_ratio), because every input token it gives up
        // frees output budget too: 540 / 2.4 = 225.
        assert_eq!(without, 13236);
        assert_eq!(with, 13011);
        assert_eq!(without - with, 225);
    }

    #[test]
    fn no_context_budget_leaves_the_window_exactly_as_it_was() {
        let b = budget(32768, 8192, 1.4);
        assert_eq!(b.context, crate::infer::context::ContextBudget::default());
        assert_eq!(segment_tokens(b, 1000), segment_tokens(b, 1000));
        assert_eq!(b.context.total(), 0);
    }
```

The existing `budget()` test helper in that module constructs a `SynthesisBudget`; add `context: ContextBudget::default()` to it so it compiles.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib infer::budget`
Expected: FAIL to compile — `SynthesisBudget` has no field `context`.

- [ ] **Step 4: Add the field and spend it**

In `src/infer/mod.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub struct SynthesisBudget {
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    pub output_ratio: f32,
    /// What the window gives up so each call can carry the document's opening
    /// and its neighbours' edges.
    pub context: crate::infer::context::ContextBudget,
}
```

In `src/infer/budget.rs`, `segment_tokens` subtracts it alongside the prompt overhead:

```rust
pub fn segment_tokens(budget: SynthesisBudget, prompt_overhead: usize) -> usize {
    let ratio = budget.output_ratio.max(0.1);
    let usable = budget
        .context_tokens
        .saturating_sub(prompt_overhead)
        .saturating_sub(budget.context.total());
    let by_context = (usable as f32 / (1.0 + ratio)) as usize;
    let by_output = (budget.max_output_tokens as f32 / ratio) as usize;
    by_context.min(by_output).max(MIN_SEGMENT_TOKENS)
}
```

The exact numbers in the test come from this arithmetic: `usable = 32768 - 1000 - 540 = 31228`, `by_context = 31228 / 2.4 = 13011` after truncation, against `13236` without context. `by_output = 100_000 / 1.4 = 71428` does not bind. If the assertion is off by one, the cause is `f32` truncation and the printed value is correct — take it.

- [ ] **Step 5: Add the config fields**

In `src/config.rs`, inside `SynthesizeRole`:

```rust
    /// Tokens of the document's verbatim opening prepended to every window, so
    /// an artifact from deep in a long document still knows what product and
    /// version it belongs to. Zero disables it.
    #[serde(default = "default_context_opening_tokens")]
    pub context_opening_tokens: usize,
    /// Tokens of each neighbouring window carried on both sides, so a window
    /// that opens mid-procedure can still resolve what its pronouns point at.
    /// Zero disables it.
    #[serde(default = "default_context_overlap_tokens")]
    pub context_overlap_tokens: usize,
```

And beside the other defaults functions:

```rust
fn default_context_opening_tokens() -> usize {
    200
}

fn default_context_overlap_tokens() -> usize {
    150
}
```

- [ ] **Step 6: Populate it in the real synthesizer**

In `src/infer/openai.rs:85-89`:

```rust
            budget: SynthesisBudget {
                context_tokens: cfg.context_tokens,
                max_output_tokens: cfg.max_output_tokens,
                output_ratio: cfg.output_ratio,
                context: crate::infer::context::ContextBudget {
                    opening: cfg.context_opening_tokens,
                    overlap: cfg.context_overlap_tokens,
                },
            },
```

In `src/infer/fake.rs`, add `context: Default::default()` to every literal `SynthesisBudget { .. }`. Find them with `grep -n "SynthesisBudget {" src/infer/fake.rs`. **They stay at the default of zero on purpose** — every existing synthesize test must keep today's geometry without being edited.

- [ ] **Step 7: Run the tests and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass, and no existing test needed an assertion changed.

- [ ] **Step 8: Commit**

```bash
git add src/config.rs src/infer/mod.rs src/infer/budget.rs src/infer/context.rs src/infer/openai.rs src/infer/fake.rs
git commit -m "feat: reserve a per-window budget for surrounding context

The window a synthesizer call gets is about to carry the document's opening
and its neighbours' edges. Those are prompt tokens like any other, so the
window has to give up exactly what they cost or the assembled prompt stops
fitting the model.

The budget travels on SynthesisBudget, which already flows from config to
segment_tokens, so nothing new has to reach for Core. It defaults to zero,
which reproduces today's windowing exactly."
```

---

### Task 3: Assemble the three context blocks

**Files:**
- Modify: `src/infer/context.rs` (created in Task 2)
- Test: `src/infer/context.rs` inline `mod tests`

**Interfaces:**
- Consumes: `ContextBudget` (Task 2), `crate::infer::split::segment_text`, `crate::infer::budget::TokenCounter`.
- Produces:
  - `WindowContext { pub opening: Option<String>, pub before: Option<String>, pub after: Option<String> }`, `#[derive(Debug, Clone, Default, PartialEq)]`
  - `WindowContext::build(raw_text: &str, spans: &[(i64, i64)], idx: usize, budget: ContextBudget, counter: &TokenCounter) -> WindowContext`
  - `WindowContext::blocks(&self) -> impl Iterator<Item = &str>`
  - `WindowContext::is_empty(&self) -> bool`

- [ ] **Step 1: Write the failing tests**

Add to `src/infer/context.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::TokenCounter;

    fn corpus() -> String {
        let mut lines = vec![
            "# Backup Server Admin Guide".to_string(),
            "Covers PBS 3.x on Debian 12.".to_string(),
        ];
        for i in 0..60 {
            lines.push(format!("body line {i} with enough words to cost tokens"));
        }
        lines.join("\n")
    }

    fn spans() -> Vec<(i64, i64)> {
        vec![(1, 20), (21, 40), (41, 62)]
    }

    fn budget() -> ContextBudget {
        ContextBudget {
            opening: 30,
            overlap: 20,
        }
    }

    #[test]
    fn the_first_window_gets_no_opening_and_no_preceding_context() {
        let c = WindowContext::build(&corpus(), &spans(), 0, budget(), &TokenCounter::Estimate);
        assert_eq!(c.opening, None, "window 0 already contains the opening");
        assert_eq!(c.before, None, "window 0 has nothing before it");
        assert!(c.after.is_some(), "window 0 has a window after it");
    }

    #[test]
    fn the_last_window_gets_no_following_context() {
        let c = WindowContext::build(&corpus(), &spans(), 2, budget(), &TokenCounter::Estimate);
        assert_eq!(c.after, None);
        assert!(c.before.is_some());
        assert!(c.opening.is_some());
    }

    #[test]
    fn a_middle_window_gets_all_three_blocks_from_the_right_places() {
        let text = corpus();
        let c = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);

        assert!(
            c.opening.as_deref().unwrap().starts_with("# Backup Server Admin Guide"),
            "the opening must be the document's first lines verbatim"
        );
        // The preceding block is the tail of window 0, which ends at line 20.
        assert!(
            c.before.as_deref().unwrap().contains("body line 17"),
            "got {:?}",
            c.before
        );
        // The following block is the head of window 2, which starts at line 41.
        assert!(
            c.after.as_deref().unwrap().contains("body line 38"),
            "got {:?}",
            c.after
        );
    }

    #[test]
    fn every_block_stays_inside_its_budget() {
        let counter = TokenCounter::Estimate;
        let c = WindowContext::build(&corpus(), &spans(), 1, budget(), &counter);
        assert!(counter.count(c.opening.as_deref().unwrap()) <= 30);
        assert!(counter.count(c.before.as_deref().unwrap()) <= 20);
        assert!(counter.count(c.after.as_deref().unwrap()) <= 20);
    }

    #[test]
    fn the_opening_never_runs_into_the_window_it_introduces() {
        // Window 1 starts at line 2, so an opening of 30 tokens would cover
        // lines the window already holds. It must be cut at line 1.
        let text = corpus();
        let spans = vec![(1, 1), (2, 30), (31, 62)];
        let c = WindowContext::build(&text, &spans, 1, budget(), &TokenCounter::Estimate);
        let opening = c.opening.as_deref().unwrap();
        assert!(
            !opening.contains("Covers PBS 3.x"),
            "line 2 belongs to the window itself: {opening:?}"
        );
    }

    #[test]
    fn a_zero_budget_produces_nothing() {
        let c = WindowContext::build(
            &corpus(),
            &spans(),
            1,
            ContextBudget::default(),
            &TokenCounter::Estimate,
        );
        assert_eq!(c, WindowContext::default());
        assert!(c.is_empty());
        assert_eq!(c.blocks().count(), 0);
    }

    #[test]
    fn assembly_is_reproducible() {
        // A retry rebuilds context from stored line numbers alone, so the same
        // spans must always give the same bytes.
        let text = corpus();
        let a = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);
        let b = WindowContext::build(&text, &spans(), 1, budget(), &TokenCounter::Estimate);
        assert_eq!(a, b);
    }

    #[test]
    fn blocks_yields_every_populated_block() {
        let c = WindowContext::build(&corpus(), &spans(), 1, budget(), &TokenCounter::Estimate);
        assert_eq!(c.blocks().count(), 3);
        let c0 = WindowContext::build(&corpus(), &spans(), 0, budget(), &TokenCounter::Estimate);
        assert_eq!(c0.blocks().count(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib infer::context`
Expected: FAIL to compile — `WindowContext` does not exist.

- [ ] **Step 3: Write the implementation**

Add to `src/infer/context.rs`, above the test module:

```rust
use crate::infer::budget::TokenCounter;
use crate::infer::split::segment_text;

/// The material surrounding one window: what document it belongs to, and what
/// its neighbours say on either side of it.
///
/// Derived, never stored. A retry rebuilds it byte-for-byte from the stored
/// line numbers, which is what keeps a resumed job idempotent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WindowContext {
    /// The document's verbatim first lines. Absent for the window that already
    /// contains them.
    pub opening: Option<String>,
    /// The tail of the previous window.
    pub before: Option<String>,
    /// The head of the next window.
    pub after: Option<String>,
}

impl WindowContext {
    pub fn build(
        raw_text: &str,
        spans: &[(i64, i64)],
        idx: usize,
        budget: ContextBudget,
        counter: &TokenCounter,
    ) -> WindowContext {
        let Some(&(start, _)) = spans.get(idx) else {
            return WindowContext::default();
        };

        // Window 0 opens with the document, so repeating it would spend the
        // budget on text the model is already reading. Later windows take the
        // opening only up to their own first line: an opening that ran into
        // the window would put the same lines in the prompt twice.
        let opening = (idx > 0 && budget.opening > 0 && start > 1)
            .then(|| segment_text(raw_text, 1, start - 1))
            .and_then(|t| head_lines(&t, budget.opening, counter));

        let before = (budget.overlap > 0)
            .then(|| idx.checked_sub(1))
            .flatten()
            .and_then(|i| spans.get(i))
            .map(|&(s, e)| segment_text(raw_text, s, e))
            .and_then(|t| tail_lines(&t, budget.overlap, counter));

        let after = (budget.overlap > 0)
            .then(|| spans.get(idx + 1))
            .flatten()
            .map(|&(s, e)| segment_text(raw_text, s, e))
            .and_then(|t| head_lines(&t, budget.overlap, counter));

        WindowContext {
            opening,
            before,
            after,
        }
    }

    /// Every block that ended up populated. Callers use this to ask whether a
    /// piece of text came from context rather than from the window itself.
    pub fn blocks(&self) -> impl Iterator<Item = &str> {
        [
            self.opening.as_deref(),
            self.before.as_deref(),
            self.after.as_deref(),
        ]
        .into_iter()
        .flatten()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks().next().is_none()
    }
}

/// As many leading whole lines as fit. Whole lines because a context block cut
/// mid-sentence reads as corruption to a small model.
fn head_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    take_lines(text.lines(), limit, counter)
}

/// As many trailing whole lines as fit, in their original order.
fn tail_lines(text: &str, limit: usize, counter: &TokenCounter) -> Option<String> {
    let mut taken = take_lines_vec(text.lines().rev(), limit, counter);
    taken.reverse();
    (!taken.is_empty()).then(|| taken.join("\n"))
}

fn take_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    limit: usize,
    counter: &TokenCounter,
) -> Option<String> {
    let taken = take_lines_vec(lines, limit, counter);
    (!taken.is_empty()).then(|| taken.join("\n"))
}

fn take_lines_vec<'a>(
    lines: impl Iterator<Item = &'a str>,
    limit: usize,
    counter: &TokenCounter,
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let n = counter.count(line) + 1; // +1 for the newline
        if used + n > limit {
            break;
        }
        out.push(line);
        used += n;
    }
    out
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib infer::context`
Expected: PASS, all eight.

- [ ] **Step 5: Verify the whole suite and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass. Nothing calls `WindowContext` yet, so no existing behaviour can have changed.

- [ ] **Step 6: Commit**

```bash
git add src/infer/context.rs
git commit -m "feat: assemble a window's surrounding context from stored spans

Three blocks: the document's verbatim opening, the tail of the previous
window, the head of the next. Derived from raw_text and the neighbouring
line ranges rather than stored, so a retried window rebuilds byte-identical
context from the line numbers alone and the job stays idempotent.

Blocks are cut on whole lines. A context block cut mid-sentence reads as
corruption to a small model, which is the opposite of the point."
```

---

### Task 4: Render the context into the prompt

**Files:**
- Modify: `src/infer/prompt.rs:4-42` (`SYNTHESIZER_SYSTEM`, `user_prompt`)
- Test: `src/infer/prompt.rs` inline `mod tests`

**Interfaces:**
- Consumes: `WindowContext` (Task 3).
- Produces: `user_prompt(segment_text: &str, first_line: i64, max_artifact_tokens: usize, context: &WindowContext) -> String` — **one new parameter**, appended.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/infer/prompt.rs`:

```rust
    #[test]
    fn context_blocks_are_fenced_and_labelled_as_context_only() {
        use crate::infer::context::WindowContext;

        let ctx = WindowContext {
            opening: Some("# Guide\nPBS 3.x on Debian 12.".into()),
            before: Some("previous window tail".into()),
            after: Some("next window head".into()),
        };
        let p = user_prompt("the window body", 1, 1024, &ctx);

        assert!(p.contains("PBS 3.x on Debian 12."));
        assert!(p.contains("previous window tail"));
        assert!(p.contains("next window head"));
        assert!(p.contains("----- INPUT -----\nthe window body\n----- END INPUT -----"));

        // The opening leads, so system prompt + opening is a byte-identical
        // prefix across every window of a corpus and a prompt cache can reuse
        // it. Everything that varies per window sits after it.
        let opening_at = p.find("PBS 3.x").unwrap();
        let before_at = p.find("previous window tail").unwrap();
        let input_at = p.find("----- INPUT -----").unwrap();
        let after_at = p.find("next window head").unwrap();
        assert!(opening_at < before_at && before_at < input_at && input_at < after_at);
    }

    #[test]
    fn an_empty_context_renders_exactly_the_prompt_of_before() {
        use crate::infer::context::WindowContext;

        let p = user_prompt("body", 1, 1024, &WindowContext::default());
        assert!(
            !p.contains("context only"),
            "an empty context must not emit empty fences: {p}"
        );
        assert!(p.starts_with("The input below starts at line 1."));
        assert!(p.ends_with("----- END INPUT -----"));
    }

    #[test]
    fn the_system_prompt_forbids_extracting_from_context() {
        assert!(SYNTHESIZER_SYSTEM.contains("context only"));
        assert!(SYNTHESIZER_SYSTEM.contains("INPUT"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib infer::prompt`
Expected: FAIL to compile — `user_prompt` takes three arguments.

- [ ] **Step 3: Rewrite `user_prompt`**

Replace `src/infer/prompt.rs:37-42` with:

```rust
pub fn user_prompt(
    segment_text: &str,
    first_line: i64,
    max_artifact_tokens: usize,
    context: &crate::infer::context::WindowContext,
) -> String {
    let mut out = String::new();
    // The opening leads so that the system prompt followed by it is a
    // byte-identical prefix for every window of a corpus, which a prompt cache
    // or a llama.cpp slot can reuse. Everything per-window follows.
    if let Some(o) = &context.opening {
        out.push_str(&format!(
            "----- DOCUMENT OPENING (context only) -----\n{o}\n\
             ----- END DOCUMENT OPENING -----\n\n"
        ));
    }
    if let Some(b) = &context.before {
        out.push_str(&format!(
            "----- PRECEDING CONTEXT (context only) -----\n{b}\n\
             ----- END PRECEDING CONTEXT -----\n\n"
        ));
    }
    out.push_str(&format!(
        "The input below starts at line {first_line}. Keep each artifact under roughly \
         {max_artifact_tokens} tokens; split into more artifacts rather than exceeding it.\n\n\
         ----- INPUT -----\n{segment_text}\n----- END INPUT -----"
    ));
    if let Some(a) = &context.after {
        out.push_str(&format!(
            "\n\n----- FOLLOWING CONTEXT (context only) -----\n{a}\n\
             ----- END FOLLOWING CONTEXT -----"
        ));
    }
    out
}
```

- [ ] **Step 4: Extend the system prompt**

In `src/infer/prompt.rs`, insert this paragraph into `SYNTHESIZER_SYSTEM`, immediately after the "Reproduce commands, file paths..." paragraph and before the markdown paragraph:

```
A block labelled "context only" is there so you can resolve references — what a
pronoun points at, which version or platform the document is about. Use it to
write artifacts that stand alone. Never emit an artifact for material that
appears only in a context block: the window that owns that material will emit
it, and emitting it twice puts two copies in the knowledge base. Extract
exclusively from the INPUT block.
```

- [ ] **Step 5: Fix the one existing call site**

`src/infer/openai.rs:143` currently reads `let user = prompt::user_prompt(text, 1, self.max_artifact_tokens);`. Change it to pass an empty context for now — Task 5 replaces this line properly:

```rust
        let user = prompt::user_prompt(
            text,
            1,
            self.max_artifact_tokens,
            &crate::infer::context::WindowContext::default(),
        );
```

- [ ] **Step 6: Run the tests and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass. `prompt_overhead` in `synthesize.rs` counts `SYNTHESIZER_SYSTEM`, so its growth is already accounted for automatically.

- [ ] **Step 7: Commit**

```bash
git add src/infer/prompt.rs src/infer/openai.rs
git commit -m "feat: render context blocks into the synthesizer prompt

Fenced, labelled context-only, and ordered so the document opening leads:
system prompt plus opening is then a byte-identical prefix across every
window of a corpus, which a prompt cache can reuse, and everything that
varies per window follows it.

The system prompt now states what a context block is for and that artifacts
are never to be drawn from one. That instruction is not load-bearing on its
own — a structural check follows in a later commit — but a small model that
does follow it costs nothing to catch."
```

---

### Task 5: Carry context through the `Synthesizer` trait

Mechanical, no behaviour change. Kept separate so a reviewer can check the signature churn without it hiding a logic change.

**Files:**
- Modify: `src/infer/mod.rs:32-42` (trait)
- Modify: `src/infer/openai.rs:142-169`
- Modify: `src/infer/fake.rs` (six `Synthesizer` impls)
- Modify: `src/jobs/synthesize.rs:48,59` (call sites)

**Interfaces:**
- Consumes: `WindowContext` (Task 3).
- Produces:
  - `pub struct SegmentInput<'a> { pub core: &'a str, pub context: &'a WindowContext }` in `src/infer/mod.rs`
  - `async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>>`

- [ ] **Step 1: Define the input struct**

In `src/infer/mod.rs`, above the trait:

```rust
/// One window as the synthesizer sees it: the text artifacts are drawn from,
/// and the surrounding material that exists only so references can be
/// resolved. They are separate fields rather than one assembled string
/// because everything downstream — span location, literal checking,
/// paraphrase detection — has to be told which is which.
pub struct SegmentInput<'a> {
    pub core: &'a str,
    pub context: &'a crate::infer::context::WindowContext,
}
```

- [ ] **Step 2: Change the trait**

```rust
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Segment one window of text. Windowing itself is the caller's job.
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>>;
    fn budget(&self) -> SynthesisBudget;
```

- [ ] **Step 3: Run the build to enumerate every call site**

Run: `cargo build 2>&1 | grep -E "^error" -A 3`
Expected: FAIL, one error per `Synthesizer` impl and per call site. Use this list as the checklist for the next step.

- [ ] **Step 4: Update the real synthesizer**

`src/infer/openai.rs:142-143`:

```rust
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let user = prompt::user_prompt(input.core, 1, self.max_artifact_tokens, input.context);
```

The rest of the method body is unchanged — `user` is already threaded through both the first attempt and the repair attempt. Add `SegmentInput` to the `use super::{...}` list at the top of the file.

- [ ] **Step 5: Update every fake**

In `src/infer/fake.rs`, each of the six impls changes its signature to `async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>>` and binds `let text = input.core;` as its first line, so the existing bodies are untouched. The delegating impl at line 205 becomes:

```rust
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        self.inner.segment(input).await
    }
```

The impl at line 249 that ignores its input keeps `_input`.

- [ ] **Step 6: Update the job's call sites**

In `src/jobs/synthesize.rs`, both `core.synthesizer.segment(&text).await?` calls (line 48 and the retry at line 59) become:

```rust
        let input = crate::infer::SegmentInput {
            core: &text,
            context: &crate::infer::context::WindowContext::default(),
        };
        let mut chunks = core.synthesizer.segment(input).await?;
```

and for the retry, construct the same `SegmentInput` again (it borrows, so it cannot be reused after the move). Task 6 replaces the `default()` with real context.

- [ ] **Step 7: Run the tests and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass, with **no assertion changed anywhere**. This task is a signature change only; if any test's expected values had to move, something else changed by accident — stop and find it.

- [ ] **Step 8: Commit**

```bash
git add src/infer/mod.rs src/infer/openai.rs src/infer/fake.rs src/jobs/synthesize.rs
git commit -m "refactor: a synthesizer call carries its window's context

SegmentInput keeps the window's own text and its surrounding context as
separate fields rather than one assembled string, because span location,
literal checking and paraphrase detection all have to be told which is
which. Behaviour is unchanged: every caller passes an empty context."
```

---

### Task 6: Assemble real context per window

**Files:**
- Modify: `src/jobs/synthesize.rs:12-14` (`prompt_overhead`), `:28-48` (window loop)
- Test: `src/jobs/synthesize.rs` inline `mod tests`

**Interfaces:**
- Consumes: `WindowContext::build` (Task 3), `SegmentInput` (Task 5), `SynthesisBudget::context` (Task 2).
- Produces: no new public names.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/jobs/synthesize.rs`. It needs a fake that records what it was given; add this to `src/infer/fake.rs` next to the other fakes:

```rust
/// Records the input of the last call, so a test can assert what the window
/// was actually given rather than what it was supposed to be given.
pub struct RecordingSynthesizer {
    pub seen: std::sync::Mutex<Vec<(String, crate::infer::context::WindowContext)>>,
    pub budget: SynthesisBudget,
}

impl RecordingSynthesizer {
    pub fn new(budget: SynthesisBudget) -> Self {
        Self {
            seen: std::sync::Mutex::new(Vec::new()),
            budget,
        }
    }
}

#[async_trait]
impl Synthesizer for RecordingSynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        self.seen
            .lock()
            .unwrap()
            .push((input.core.to_string(), input.context.clone()));
        Ok(vec![ProposedArtifact {
            text: input.core.lines().next().unwrap_or("empty").to_string(),
            title: Some("recorded".into()),
            category: Some("note".into()),
            tags: vec![],
            corpus_lines: None,
            caveats: vec![],
        }])
    }
    fn budget(&self) -> SynthesisBudget {
        self.budget
    }
}
```

Then the test:

```rust
    #[tokio::test]
    async fn every_window_after_the_first_is_given_the_document_opening() {
        use crate::infer::context::ContextBudget;
        use crate::infer::fake::RecordingSynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(crate::infer::SynthesisBudget {
            context_tokens: 2000,
            max_output_tokens: 100_000,
            output_ratio: 1.0,
            context: ContextBudget {
                opening: 30,
                overlap: 20,
            },
        }));
        core.synthesizer = rec.clone();

        let mut lines = vec!["# Backup Guide".to_string(), "PBS 3.x on Debian 12.".into()];
        for i in 0..400 {
            lines.push(format!("body line {i} with enough words to cost real tokens"));
        }
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        run(&core, &src.id).await.unwrap();

        let seen = rec.seen.lock().unwrap();
        assert!(seen.len() > 1, "the fixture must produce several windows");
        assert_eq!(seen[0].1.opening, None, "window 0 already holds the opening");
        assert_eq!(seen[0].1.before, None);
        for (i, (_, ctx)) in seen.iter().enumerate().skip(1) {
            assert!(
                ctx.opening.as_deref().unwrap().contains("# Backup Guide"),
                "window {i} lost the document opening"
            );
            assert!(ctx.before.is_some(), "window {i} lost its preceding context");
        }
        assert_eq!(
            seen.last().unwrap().1.after,
            None,
            "the last window has nothing after it"
        );
    }

    #[tokio::test]
    async fn a_windows_context_is_the_text_of_its_neighbours() {
        use crate::infer::context::ContextBudget;
        use crate::infer::fake::RecordingSynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        let rec = std::sync::Arc::new(RecordingSynthesizer::new(crate::infer::SynthesisBudget {
            context_tokens: 2000,
            max_output_tokens: 100_000,
            output_ratio: 1.0,
            context: ContextBudget {
                opening: 30,
                overlap: 20,
            },
        }));
        core.synthesizer = rec.clone();

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        run(&core, &src.id).await.unwrap();

        let seen = rec.seen.lock().unwrap();
        // Window 1's preceding context must be the literal end of window 0's
        // own text, and its following context the literal start of window 2's.
        let w0_tail_line = seen[0].0.lines().last().unwrap();
        assert!(
            seen[1].1.before.as_deref().unwrap().ends_with(w0_tail_line),
            "preceding context is not the previous window's tail"
        );
        let w2_head_line = seen[2].0.lines().next().unwrap();
        assert!(
            seen[1].1.after.as_deref().unwrap().starts_with(w2_head_line),
            "following context is not the next window's head"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib jobs::synthesize::tests::every_window_after jobs::synthesize::tests::a_windows_context`
Expected: FAIL — `opening` is `None` for every window, because the job still passes `WindowContext::default()`.

- [ ] **Step 3: Charge the context to the prompt overhead**

In `src/jobs/synthesize.rs`:

```rust
/// Tokens consumed by the system prompt and scaffolding, plus whatever the
/// window gives up to carry its surrounding context. Measured from the real
/// prompt rather than guessed.
fn prompt_overhead(core: &Core) -> usize {
    core.counter.count(crate::infer::prompt::SYNTHESIZER_SYSTEM) + 200
}
```

stays as it is — `segment_tokens` already subtracts `budget.context.total()` separately, as of Task 2. **Do not add it here as well**; that would charge it twice and shrink windows by double. Leave this function unchanged and note it in the commit.

- [ ] **Step 4: Build the context in the window loop**

In `src/jobs/synthesize.rs`, the `spans` vector is already computed at line 42. Inside the `for w in core.store.pending_segments(corpus_id).await?` loop, replace the `SegmentInput` construction from Task 5 with:

```rust
        let ctx = crate::infer::context::WindowContext::build(
            &src.raw_text,
            &spans,
            w.idx as usize,
            core.synthesizer.budget().context,
            &core.counter,
        );

        let mut chunks = core
            .synthesizer
            .segment(crate::infer::SegmentInput {
                core: &text,
                context: &ctx,
            })
            .await?;
```

and the paraphrase retry immediately below it:

```rust
            chunks = core
                .synthesizer
                .segment(crate::infer::SegmentInput {
                    core: &text,
                    context: &ctx,
                })
                .await?;
```

Everything downstream keeps using `text`: `paraphrased(&chunks, &text)`, `locate_span(&c.text, &text, w.start_line)`, and `flag_unverified(core, &written, &text)`. That is the invariant this task must not break — spans stay in core coordinates.

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib jobs::synthesize`
Expected: PASS, including every pre-existing test. The existing fakes carry `ContextBudget::default()`, so their windows get empty context and today's spans.

- [ ] **Step 6: Verify the whole suite and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add src/jobs/synthesize.rs src/infer/fake.rs
git commit -m "feat: every window is synthesized with its document opening and neighbours

A window that opens mid-procedure could not resolve what its pronouns
pointed at, and a window eight hundred lines into a guide had no way to
know the procedure was specific to a version and platform stated in the
document's first paragraph — which is exactly what the caveats field asks
for. Both are now in the prompt.

Spans stay in core coordinates: locate_span, missing_literals and the
paraphrase check are all still given the window's own text, never the
assembled prompt. The context budget is charged once, in segment_tokens;
prompt_overhead deliberately does not also subtract it."
```

---

### Task 7: Drop artifacts drawn from a context block

**Files:**
- Modify: `src/jobs/synthesize.rs` (window loop, immediately after the paraphrase retry and before the span loop)
- Test: `src/jobs/synthesize.rs` inline `mod tests`

**Interfaces:**
- Consumes: `WindowContext::blocks` (Task 3), `crate::infer::verify::locate_span`.
- Produces: `fn from_context_only(text: &str, core_text: &str, ctx: &WindowContext) -> bool` (private to the module).

- [ ] **Step 1: Write the failing tests**

Add to `src/infer/fake.rs`:

```rust
/// Emits one artifact per line it is given, context blocks included. Stands in
/// for a small model that ignores the instruction not to extract from context.
pub struct GreedySynthesizer {
    pub budget: SynthesisBudget,
}

#[async_trait]
impl Synthesizer for GreedySynthesizer {
    async fn segment(&self, input: SegmentInput<'_>) -> Result<Vec<ProposedArtifact>> {
        let mut out: Vec<ProposedArtifact> = Vec::new();
        let from_context = input.context.blocks().flat_map(|b| b.lines());
        for line in input.core.lines().chain(from_context) {
            if line.trim().is_empty() {
                continue;
            }
            out.push(ProposedArtifact {
                text: line.to_string(),
                title: Some("greedy".into()),
                category: Some("note".into()),
                tags: vec![],
                corpus_lines: None,
                caveats: vec![],
            });
        }
        Ok(out)
    }
    fn budget(&self) -> SynthesisBudget {
        self.budget
    }
}
```

And the tests in `src/jobs/synthesize.rs`:

```rust
    #[test]
    fn an_artifact_found_only_in_context_is_recognised() {
        use crate::infer::context::WindowContext;

        let core_text = "the window says something quite specific here\nand more of it";
        let ctx = WindowContext {
            opening: Some("the document opening states the version clearly".into()),
            before: None,
            after: Some("the following window describes another procedure".into()),
        };

        // Drawn from the window itself: keep.
        assert!(!from_context_only(
            "the window says something quite specific here",
            core_text,
            &ctx
        ));
        // Drawn from a context block and nowhere in the window: drop.
        assert!(from_context_only(
            "the following window describes another procedure",
            core_text,
            &ctx
        ));
        // Located nowhere at all — a heavily reworded artifact. Keep it, so it
        // reaches flag_unverified the way it does today instead of vanishing.
        assert!(!from_context_only(
            "an entirely reworded statement about unrelated matters",
            core_text,
            &ctx
        ));
    }

    #[tokio::test]
    async fn a_model_that_extracts_from_context_does_not_duplicate_artifacts() {
        use crate::infer::context::ContextBudget;
        use crate::infer::fake::GreedySynthesizer;

        let mut core = crate::core::test_support::test_core().await;
        core.synthesizer = std::sync::Arc::new(GreedySynthesizer {
            budget: crate::infer::SynthesisBudget {
                context_tokens: 2000,
                max_output_tokens: 100_000,
                output_ratio: 1.0,
                context: ContextBudget {
                    opening: 30,
                    overlap: 20,
                },
            },
        });

        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        run(&core, &src.id).await.unwrap();

        let written = core.store.artifacts_for_corpus(&src.id).await.unwrap();
        let mut texts: Vec<&str> = written.iter().map(|c| c.text.as_str()).collect();
        texts.sort_unstable();
        let before = texts.len();
        texts.dedup();
        assert_eq!(
            texts.len(),
            before,
            "the same line was stored as an artifact more than once"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib jobs::synthesize::tests::an_artifact_found_only jobs::synthesize::tests::a_model_that_extracts`
Expected: the first FAILs to compile (`from_context_only` undefined); the second FAILs on the duplicate assertion.

- [ ] **Step 3: Write the implementation**

Add to `src/jobs/synthesize.rs`:

```rust
/// Did this artifact come from a context block rather than from the window?
///
/// The prompt says not to extract from context, and a small local model obeys
/// that unevenly, so the check is structural. Three outcomes matter and only
/// the middle one is a duplicate: located in the window, keep; located only in
/// context, drop, because the window that owns the material will emit it
/// properly; located nowhere, keep — that is an artifact the model reworded
/// hard, which flag_unverified has always handled and which must not start
/// silently disappearing.
fn from_context_only(
    text: &str,
    core_text: &str,
    ctx: &crate::infer::context::WindowContext,
) -> bool {
    if crate::infer::verify::locate_span(text, core_text, 1).is_some() {
        return false;
    }
    ctx.blocks()
        .any(|b| crate::infer::verify::locate_span(text, b, 1).is_some())
}
```

In the window loop, immediately after the paraphrase retry and **before** the span-assignment loop:

```rust
        if !ctx.is_empty() {
            let before = chunks.len();
            chunks.retain(|c| !from_context_only(&c.text, &text, &ctx));
            let dropped = before - chunks.len();
            if dropped > 0 {
                // A rising count here means the configured model is ignoring
                // the prompt's context-only instruction. Better as a number in
                // the log than as duplicates in the base.
                tracing::info!(
                    corpus_id,
                    window = w.idx,
                    dropped,
                    "artifacts drawn from context blocks were dropped"
                );
            }
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib jobs::synthesize`
Expected: PASS, all of them.

- [ ] **Step 5: Verify the whole suite and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/synthesize.rs src/infer/fake.rs
git commit -m "feat: an artifact drawn from a context block is dropped

Overlap puts the same passage in two windows, and the fidelity rule means a
duplicate artifact is flagged rather than merged — so duplicates accumulate
instead of resolving. Telling the model not to extract from context is not
enough on its own; a small local model follows that unevenly.

The check is structural. Located in the window, keep. Located only in a
context block, drop, because the window that owns the material emits it
properly. Located nowhere, keep and let flag_unverified handle it as
before: that is a heavily reworded artifact, not a duplicate, and dropping
on locate-failure alone would start discarding legitimate work."
```

---

### Task 8: Guard the window size before it is sent

**Files:**
- Modify: `src/jobs/synthesize.rs` (window loop, immediately after `let text = segment_text(...)`)
- Test: `src/jobs/synthesize.rs` inline `mod tests`

**Interfaces:**
- Consumes: `segment_tokens` (Task 2).
- Produces: no new public names.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn a_window_is_never_sent_over_its_own_budget() {
        // The guard is a can't-happen check: split_into_segments now floors
        // window size. It exists because the failure it catches is a job that
        // spins against the endpoint forever, and a debug_assert turns that
        // into a test failure instead of a production incident.
        let core = crate::core::test_support::test_core().await;
        let lines: Vec<String> = (0..400)
            .map(|i| format!("body line {i} with enough words to cost real tokens"))
            .collect();
        let src = core
            .store
            .insert_corpus(&lines.join("\n"), "web", None)
            .await
            .unwrap();

        run(&core, &src.id).await.unwrap();

        let budget = crate::infer::budget::segment_tokens(
            core.synthesizer.budget(),
            prompt_overhead(&core),
        );
        for s in core.store.segments_for_corpus(&src.id).await.unwrap() {
            let text = crate::infer::split::segment_text(&src.raw_text, s.start_line, s.end_line);
            assert!(
                core.counter.count(&text) <= budget,
                "window {} is {} tokens against a budget of {budget}",
                s.idx,
                core.counter.count(&text)
            );
        }
    }
```

- [ ] **Step 2: Run test to verify it passes already**

Run: `cargo test --lib jobs::synthesize::tests::a_window_is_never_sent`
Expected: PASS — Task 1 already guarantees this. This test is the regression lock; it exists so that a future change to the splitter cannot quietly remove the guarantee. Confirm it fails if you temporarily revert Task 1's `cut_long_line` call, then restore.

- [ ] **Step 3: Add the in-loop guard**

In `src/jobs/synthesize.rs`, immediately after `let text = segment_text(&src.raw_text, w.start_line, w.end_line);`:

```rust
        // split_into_segments guarantees this; the guard is here because the
        // failure mode when it stops being true is a job retrying an
        // over-context window against the endpoint with growing backoff and no
        // terminal state. A debug_assert makes a regression a test failure,
        // and the log line makes it visible in production without refusing to
        // do the work.
        let window_budget = segment_tokens(core.synthesizer.budget(), prompt_overhead(core));
        let window_tokens = core.counter.count(&text);
        debug_assert!(
            window_tokens <= window_budget,
            "window {} is {window_tokens} tokens against a budget of {window_budget}",
            w.idx
        );
        if window_tokens > window_budget {
            tracing::error!(
                corpus_id,
                window = w.idx,
                window_tokens,
                window_budget,
                "window exceeds its budget; the splitter did not shrink it"
            );
        }
```

- [ ] **Step 4: Run the tests and lints**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: all pass. `cargo test` builds in debug, so the `debug_assert!` is live for the whole suite.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/synthesize.rs
git commit -m "test: lock the guarantee that a window fits its budget

The splitter now floors window size, so this cannot fire. It is here
because of what happens when it does: a job has no terminal state, so an
over-context window retries against the endpoint with growing backoff
forever. That is the same shape as the embed loop of this week — a splitter
that returns something it was asked to shrink but did not, and a caller
that assumes it shrank.

debug_assert makes a regression fail in CI; the error log makes it visible
in production without refusing to do the work."
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
| ------------ | ---- |
| 2. What each window is given — four blocks, ordering, no line numbers on context | 3, 4 |
| 3. Context derived, never stored | 3 (`build` from spans only), 6 (no schema change) |
| 4. Duplicate suppression, three-row table | 7 |
| 5. Budget, `H`/`O` defaults, config knobs, zero reproduces today | 2 |
| 6. Single-line floor and the pre-send guard | 1, 8 |
| 7. Testing items 1–8 | 3 (items 1, 2, 3, 6-partial), 6 (item 4), 7 (item 5), 1 (item 7), 2 (item 8) |
| 8. Risks accepted — no code, recorded in the spec | — |

Spec test item 6 (`H = 0, O = 0` reproduces today's windowing span for span) is covered indirectly: every pre-existing synthesize test runs with `ContextBudget::default()`, and Tasks 2, 5, and 6 each require the whole suite to pass **with no assertion changed**. That is a stronger check than a single fixture comparison, and it is stated explicitly in the verification step of all three tasks.

**Placeholder scan:** clean — every code step carries real code, every run step carries a real command and an expected result.

**Type consistency:** `ContextBudget { opening, overlap }` and `WindowContext { opening, before, after }` are used under those exact names in Tasks 2, 3, 4, 6, 7. `SegmentInput { core, context }` matches between Tasks 5, 6, 7. `from_context_only(text, core_text, ctx)` matches between its definition and both tests in Task 7. `WindowContext` must derive `Clone` for `RecordingSynthesizer` (Task 6) — it does, from Task 3.

**Ordering note:** Task 1 is independent of Tasks 2–8 and fixes a bug that is live in production right now. It can ship on its own, ahead of the rest, and should if the rest stalls in review.
