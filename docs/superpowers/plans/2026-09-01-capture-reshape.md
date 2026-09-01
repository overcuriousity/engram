# Capture Reshape Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One capture pipeline (verbatim-first, size-forked), a real tokenizer, an industry chunker, one synthesis call that also judges intent/events/links/tags, the classifier and date rules deleted, and the judge deck page removed.

**Architecture:** Every capture stores verbatim passages and embeds them; a capture that fits one synthesis call arms that call immediately (reusing the promotion machinery — `keep_artifacts`, `supersede_covered`), everything larger waits for promotion as before. The synthesis call on the small path sees nearest-neighbor artifacts and returns a judgement (reminder/journal/events/links/tags) that replaces the prototype classifier, the cue tables, and the rule-based date readers. `SynthesisMode` and the judge deck die.

**Tech Stack:** Rust (axum, sqlx, askama, htmx), `tokenizers` (HF), `text-splitter`, Qdrant, SQLite.

**Spec:** `docs/superpowers/specs/2026-09-01-capture-reshape-design.md` — read it before starting any task.

## Global Constraints

- Branch: create `feat/reshape` off `feat/time` (use superpowers:using-git-worktrees at execution start).
- `[infer.synthesize]` becomes **required**; a config with `infer.synthesis = ...` or `infer.segment_tokens` must fail at startup with a message naming this reshape. No compat shims, no value mapping, no migration code (repo rule).
- KISS: the operator cuts template-shaped code. Delete dead paths in the same task that makes them dead; do not leave vestigial fields "for later".
- Artifact size cap is `effective_chunk_tokens()` (`src/config.rs:1355`) — never a hardcoded 384.
- Every task: `cargo test` green before commit. Repo tests run without Qdrant (fake vector store) except `tests/integration_qdrant.rs`.
- Commit messages follow repo style: `feat(scope): lowercase sentence describing the change`.
- Where this plan says "read the vendored source", the crate is already in `~/.cargo/registry/src/` after `cargo add` — read the actual trait/function definitions there before writing code against them; do not guess APIs.

---

### Task 1: A real tokenizer behind `TokenCounter`

**Files:**
- Modify: `Cargo.toml` (add `tokenizers`)
- Create: `assets/tokenizer.json` (vendored Qwen tokenizer)
- Modify: `src/infer/budget.rs` (TokenCounter becomes a struct with an optional real tokenizer)
- Modify: `src/config.rs` (new optional `infer.tokenizer` key on `RawInferConfig`/`InferConfig`)
- Modify: `src/core/mod.rs:346` and `:583` (construction sites: `Arc::new(TokenCounter)` → loaded counter / `Arc::new(TokenCounter::default())`)
- Modify: every `&TokenCounter` literal (34 sites, mostly tests) → `&TokenCounter::default()`

**Interfaces:**
- Consumes: nothing new.
- Produces: `TokenCounter::default()` (estimator-only), `TokenCounter::load(spec: Option<&str>, cache_dir: &Path) -> TokenCounter` (bundled default → configured path/URL → estimator fallback, never an error), `counter.count(&str) -> usize` (unchanged signature). `impl Default for TokenCounter`.

- [ ] **Step 1: Add the dependency and vendor the tokenizer file**

```bash
cargo add tokenizers@0.23 --no-default-features --features onig
cargo check 2>&1 | tail -5
```

If the `onig` feature name is rejected or its C build fails, retry with default features (`cargo add tokenizers@0.23`). Then vendor the tokenizer of the model family the example config serves (Qwen3.8 as of the spec; if that exact repo path 404s, use the newest Qwen3-series repo — the tokenizer is shared across the family):

```bash
curl -fL -o assets/tokenizer.json \
  https://huggingface.co/Qwen/Qwen3.8/resolve/main/tokenizer.json \
  || curl -fL -o assets/tokenizer.json \
  https://huggingface.co/Qwen/Qwen3-8B/resolve/main/tokenizer.json
ls -la assets/tokenizer.json   # expect a few MB
```

- [ ] **Step 2: Write the failing tests** (in `src/infer/budget.rs` `#[cfg(test)]`)

```rust
#[test]
fn the_bundled_tokenizer_counts_and_differs_from_the_estimator() {
    let real = TokenCounter::load(None, std::path::Path::new("/nonexistent-cache"));
    let est = TokenCounter::default();
    let text = "Der Bericht muss bis Freitag um 16:00 abgegeben werden.";
    assert!(real.count(text) > 0);
    // The estimator is chars*2/7; a real BPE count differs on this input.
    assert_ne!(real.count(text), est.count(text));
}

#[test]
fn a_bad_path_falls_back_to_the_estimator_instead_of_failing() {
    let c = TokenCounter::load(Some("/no/such/file.json"), std::path::Path::new("/tmp"));
    assert_eq!(c.count("hello world"), TokenCounter::default().count("hello world"));
}

#[test]
fn a_cached_url_download_is_read_from_the_cache_file() {
    // Seed the cache the way a first boot's download would, then "load" the
    // URL with no network: the cache hit is the behavior under test.
    let dir = std::env::temp_dir().join(format!("tok-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = "https://example.invalid/tokenizer.json";
    let cache = TokenCounter::cache_path(&dir, url);
    std::fs::write(&cache, include_bytes!("../../assets/tokenizer.json")).unwrap();
    let c = TokenCounter::load(Some(url), &dir);
    assert_ne!(c.count("hello world"), TokenCounter::default().count("hello world"),
        "the cached real tokenizer must be in use");
}
```

- [ ] **Step 2b: Run them** — `cargo test -p engram infer::budget` — expect compile failures (no `load`, unit struct).

- [ ] **Step 3: Implement**

In `src/infer/budget.rs`, replace the unit struct:

```rust
/// Token counts for budgets. A real tokenizer where one is loadable —
/// bundled, from a path, or from a once-downloaded URL — and the pessimistic
/// chars/3.5 estimate where none is. `Default` is the estimator alone.
#[derive(Default)]
pub struct TokenCounter {
    tok: Option<tokenizers::Tokenizer>,
}

const BUNDLED: &[u8] = include_bytes!("../../assets/tokenizer.json");

impl TokenCounter {
    pub fn count(&self, text: &str) -> usize {
        match &self.tok {
            Some(t) => t
                .encode_fast(text, false)
                .map(|e| e.len())
                .unwrap_or_else(|_| estimate(text)),
            None => estimate(text),
        }
    }

    /// Where a URL's one-time download lands: keyed by a hash of the URL so a
    /// changed link re-fetches, beside the store so it survives restarts.
    pub fn cache_path(cache_dir: &std::path::Path, url: &str) -> std::path::PathBuf {
        use sha2::{Digest, Sha256};
        let h = hex::encode(&Sha256::digest(url.as_bytes())[..8]);
        cache_dir.join(format!("tokenizer-{h}.json"))
    }

    /// Never an error: a tokenizer is an accuracy upgrade, not a reason to
    /// refuse startup. Each fallback logs what it fell back from.
    pub fn load(spec: Option<&str>, cache_dir: &std::path::Path) -> TokenCounter {
        let tok = match spec {
            Some(s) if s.starts_with("http://") || s.starts_with("https://") => {
                let cache = Self::cache_path(cache_dir, s);
                let bytes = match std::fs::read(&cache) {
                    Ok(b) => Some(b),
                    Err(_) => match reqwest::blocking::get(s).and_then(|r| r.error_for_status()) {
                        Ok(resp) => match resp.bytes() {
                            Ok(b) => {
                                let _ = std::fs::create_dir_all(cache_dir);
                                if let Err(e) = std::fs::write(&cache, &b) {
                                    tracing::warn!(error = %e, "could not cache the tokenizer; it will re-download next boot");
                                }
                                Some(b.to_vec())
                            }
                            Err(e) => { tracing::warn!(error = %e, url = s, "tokenizer download failed; estimator in use until next boot"); None }
                        },
                        Err(e) => { tracing::warn!(error = %e, url = s, "tokenizer download failed; estimator in use until next boot"); None }
                    },
                };
                bytes.and_then(|b| tokenizers::Tokenizer::from_bytes(&b)
                    .map_err(|e| tracing::warn!(error = %e, "downloaded tokenizer did not parse; estimator in use")).ok())
            }
            Some(p) => std::fs::read(p)
                .map_err(|e| tracing::warn!(error = %e, path = p, "tokenizer path unreadable; using the bundled default"))
                .ok()
                .and_then(|b| tokenizers::Tokenizer::from_bytes(&b).ok())
                .or_else(|| tokenizers::Tokenizer::from_bytes(BUNDLED).ok()),
            None => tokenizers::Tokenizer::from_bytes(BUNDLED)
                .map_err(|e| tracing::warn!(error = %e, "bundled tokenizer did not load; estimator in use")).ok(),
        };
        TokenCounter { tok }
    }
}
```

Notes for the implementer:
- Check the vendored `tokenizers` source for the exact encode call (`encode_fast` vs `encode`) and `from_bytes` signature; adjust.
- `reqwest::blocking` inside an async runtime panics — `TokenCounter::load` is called from `Core::build`, which is async context. Wrap the blocking fetch: `tokio::task::block_in_place(|| ...)` or do the fetch with the async client and `.await` in `Core::build` before constructing the counter. Pick whichever fits `Core::build`'s shape; do not let a boot panic in.
- Keep `estimate`, `MIN_SEGMENT_TOKENS`, `segment_tokens`, headroom fns unchanged.

Config (`src/config.rs`): add to `RawInferConfig` and `InferConfig`:

```rust
/// Path or http(s) URL of a HF-format tokenizer.json. A URL is fetched once
/// and cached beside the store. Unset: the bundled default (Qwen family).
#[serde(default)]
pub tokenizer: Option<String>,
```

Core (`src/core/mod.rs:346`): `counter: Arc::new(TokenCounter::load(cfg.infer.tokenizer.as_deref(), std::path::Path::new(&cfg.store.dir)))`. Test core (`:583`): `Arc::new(TokenCounter::default())` — tests stay on the estimator so every size-sensitive assertion in the tree keeps its arithmetic.

Then fix the 34 `&TokenCounter` literals:

```bash
grep -rln '&TokenCounter[^:]' src | xargs sed -i 's/&TokenCounter\([^:_]\)/\&TokenCounter::default()\1/g'
cargo check 2>&1 | head -30   # fix stragglers by hand
```

- [ ] **Step 4: Run** `cargo test` — all green (existing budget/split/passages tests still use the estimator via `default()`).

- [ ] **Step 5: Commit** — `feat(tokenizer): a real tokenizer behind TokenCounter, bundled qwen, path-or-url override`

Also add to `config.example.toml` under `[infer]`: the `tokenizer` key with the path/URL/one-time-download semantics as a comment (copy the doc comment).

---

### Task 2: text-splitter replaces the hand-rolled splitter

**Files:**
- Modify: `Cargo.toml` (add `text-splitter`, feature `markdown`)
- Rewrite: `src/infer/split.rs` (same public surface, new engine, `carry_lines` gone)
- Modify: `src/jobs/passages.rs` (`split_passages` loses the carry machinery)
- Modify: consumers of `Window.carry_lines` / `Segment.carry_lines`: `src/store/segments.rs` (drop the column from `NewSegment`/`Segment` reads — keep the DB column, write 0; no schema migration), `src/jobs/window.rs` (`resolve_span` shift, `body` derivation), `src/jobs/synthesize.rs`, `src/core/ingest.rs`, `src/core/background.rs`, `src/jobs/reconcile.rs`, `src/store/mod.rs`, `src/web/ui.rs` (grep `carry_lines`, remove each use — most become `0` or vanish)

**Interfaces:**
- Consumes: `TokenCounter` from Task 1.
- Produces: `split_into_segments(text: &str, counter: &TokenCounter, budget: usize) -> Vec<Window>` (unchanged name/args), `Window { text: String, start_line: i64, end_line: i64 }` (no `carry_lines`), `segment_text(text, start_line, end_line)` kept as-is if still referenced.

- [ ] **Step 1: Add the dep and read the API**

```bash
cargo add text-splitter@0.32 --features markdown
```

Read in the vendored source: the `ChunkSizer` trait (its one method takes `&str`, returns a size), `MarkdownSplitter::new`, `ChunkConfig::new(...).with_sizer(...)`, and `chunk_indices(text)` (yields `(byte_offset, &str)`).

- [ ] **Step 2: Write the failing tests** (replace `src/infer/split.rs` tests; keep the invariants, drop the heading-carry ones)

```rust
#[test]
fn a_text_under_budget_is_one_window_spanning_every_line() {
    let w = split_into_segments("a\nb\nc", &TokenCounter::default(), 1000);
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].start_line, w[0].end_line), (1, 3));
    assert_eq!(w[0].text, "a\nb\nc");
}

#[test]
fn windows_partition_the_line_range_without_gaps() {
    let paras: Vec<String> = (0..6).map(|i| format!("paragraph {i} words ").repeat(10)).collect();
    let text = paras.join("\n\n");
    let w = split_into_segments(&text, &TokenCounter::default(), 60);
    assert!(w.len() > 1);
    assert_eq!(w[0].start_line, 1);
    assert_eq!(w.last().unwrap().end_line, text.lines().count() as i64);
    for pair in w.windows(2) {
        assert_eq!(pair[0].end_line + 1, pair[1].start_line, "spans must abut");
    }
}

#[test]
fn markdown_headings_prefer_to_open_a_window() {
    let text = format!("## One\n{}\n## Two\n{}", "alpha words ".repeat(30), "beta words ".repeat(30));
    let w = split_into_segments(&text, &TokenCounter::default(), 60);
    assert!(w.iter().any(|w| w.text.trim_start().starts_with("## Two")), "{w:?}");
}

#[test]
fn empty_input_yields_no_windows() {
    assert!(split_into_segments("  \n \n", &TokenCounter::default(), 100).is_empty());
}
```

- [ ] **Step 3: Implement** — the new `split.rs` core:

```rust
use text_splitter::{ChunkConfig, ChunkSizer, MarkdownSplitter};

impl ChunkSizer for &TokenCounter {
    fn size(&self, chunk: &str) -> usize {  // adjust name to the trait's real method
        self.count(chunk)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
}

pub fn split_into_segments(text: &str, counter: &TokenCounter, budget: usize) -> Vec<Window> {
    if text.trim().is_empty() {
        return vec![];
    }
    let splitter = MarkdownSplitter::new(ChunkConfig::new(budget.max(1)).with_sizer(counter));
    let chunks: Vec<(usize, &str)> = splitter.chunk_indices(text).collect();
    if chunks.is_empty() {
        return vec![];
    }
    // Byte offset → 1-based line. text-splitter trims separators between
    // chunks, so a blank line can belong to neither: end_line is derived
    // from the *next* chunk's start so the ranges partition the document —
    // spans are addresses, and an unclaimed line is a line nothing renders.
    let line_of = |off: usize| text[..off].bytes().filter(|b| *b == b'\n').count() as i64 + 1;
    let total_lines = text.lines().count().max(1) as i64;
    let mut out = Vec::with_capacity(chunks.len());
    for (i, (off, body)) in chunks.iter().enumerate() {
        let start = line_of(*off);
        let end = match chunks.get(i + 1) {
            Some((next, _)) => (line_of(*next) - 1).max(start),
            None => total_lines,
        };
        out.push(Window { text: (*body).to_string(), start_line: start, end_line: end });
    }
    out
}
```

Delete: `is_heading`-based windowing loop, `cut_long_line`, `flush_buf`, `carry_lines` and every test about heading carry. Keep `segment_text` only if `grep -rn "segment_text" src` still finds callers.

Then the fallout, mechanically:
- `src/jobs/passages.rs`: `split_passages` drops `carried_heading`/`outer_heading`; title = first heading inside the passage, else the last heading seen in earlier passages of the window (keep `heading_title`, `is_heading`, `derive_title`, `strip_links` unchanged). Span math simplifies to `window.start_line + p.start_line - 1` with the same clamps. Delete the carry-specific tests; keep partition/abut/title-from-inside tests.
- `src/store/segments.rs`: `NewSegment` loses `carry_lines`; the INSERT binds `0` for the existing column (no migration).
- `src/jobs/window.rs`: `resolve_span` shift becomes `w.start_line - 1`; `body` is `w.text` directly (no skip). Update its doc comment.
- Everywhere else `grep -rn "carry_lines" src` hits: remove the read or pass 0; each site is small.

- [ ] **Step 4: Run** `cargo test` — expect churn in `passages`/`ingest`/`window` tests that asserted carried headings; fix each to the new invariant (titles still come from headings *inside* windows; only the cross-window carry is gone).

- [ ] **Step 5: Commit** — `feat(split): text-splitter is the chunker; heading carry retires`

---

### Task 3: The mode key dies; a synthesizer is required

**Files:**
- Modify: `src/config.rs` (delete `SynthesisMode`, `infer.synthesis`, `segment_tokens`, `promote.resynthesize_after_unconfirmed`; make `synthesize` required with a clear message; add legacy-key refusals; delete `warn_on_inert_settings`'s earned-specific arms)
- Modify: `src/core/mod.rs` (delete `synthesis`, `segment_tokens` fields; `synthesizer: Arc<dyn Synthesizer>` non-optional; delete `synthesizes()`)
- Modify: `src/jobs/promote.rs` (drop the mode gate in `maybe_promote`; delete `maybe_resynthesize`)
- Modify: `src/jobs/synthesize.rs` (`segment_budget` loses its `None` arm; `plan` loses the eager branch — becomes `capture_verbatim` + Task 4's fork; `finish` drops `core.synthesizes()` guard on Title)
- Modify: `src/jobs/window.rs` (the `Let Some(synth) = ...` guard becomes a plain field read)
- Modify: `src/core/test_support.rs` (test core always has a fake synthesizer; delete `synthesis`/`segment_tokens` fields)
- Modify: every test setting `core.synthesis = SynthesisMode::...` (grep; off-mode tests are rewritten to the unified pipeline or deleted where they tested the mode itself)
- Modify: `config.example.toml`, `cli.example.toml` if it names the key

**Interfaces:**
- Consumes: nothing from earlier tasks (parallel-safe after Task 2's merge).
- Produces: `Core.synthesizer: Arc<dyn Synthesizer>`; `InferConfig.synthesize: SynthesizeRole` (non-Option); no `SynthesisMode` anywhere.

- [ ] **Step 1: Write the failing config tests**

```rust
#[test]
fn a_config_still_setting_synthesis_is_refused_with_the_reshape_named() {
    let e = parse_config_str(r#"
        [infer]
        synthesis = "earned"
        [infer.tiers.t]
        base_url = "http://x/v1"
        model = "m"
        context_tokens = 32768
        max_output_tokens = 4096
        [infer.synthesize]
        tier = "t"
        [infer.embed]
        base_url = "http://x/v1"
        model = "e"
        dim = 4
        max_input_tokens = 512
    "#).unwrap_err();
    assert!(e.to_string().contains("infer.synthesis was removed"), "{e}");
}

#[test]
fn a_config_with_no_synthesize_role_is_refused() {
    let e = parse_config_str(r#"
        [infer.embed]
        base_url = "http://x/v1"
        model = "e"
        dim = 4
        max_input_tokens = 512
    "#).unwrap_err();
    assert!(e.to_string().contains("[infer.synthesize] is required"), "{e}");
}
```

(Use the file's existing config-test helper — grep `fn parse_config_str` or its equivalent in `src/config.rs` tests and match its name.)

- [ ] **Step 2: Run** — fails (the first parses fine today, the second errors differently).

- [ ] **Step 3: Implement**

- Legacy-key refusal in `Config::load`, beside `warn_on_defaulted_store` (`src/config.rs`, same `raw.get` technique):

```rust
if raw.get::<config::Value>("infer.synthesis").is_ok() {
    return Err(ConfigError::Invalid(
        "infer.synthesis was removed in the 2026-09 capture reshape: there are no modes. \
         Delete the key; [infer.synthesize] is required and capture decides per paste."
            .into(),
    ));
}
if raw.get::<config::Value>("infer.segment_tokens").is_ok() {
    return Err(ConfigError::Invalid(
        "infer.segment_tokens was removed: the window budget is always derived from \
         [infer.synthesize]'s context. Delete the key.".into(),
    ));
}
```

- `RawInferConfig`: delete `synthesis` + `segment_tokens` fields; `TryFrom` converts `synthesize: Option<...>` with `ok_or("[infer.synthesize] is required: engram cannot capture without a chat model since the 2026-09 capture reshape")`.
- `InferConfig { synthesize: SynthesizeRole, ... }`; delete `SynthesisMode`, both `default_*_tokens` fns for segment, `DEFAULT_SEGMENT_TOKENS`; keep `DEFAULT_CHUNK_TOKENS`.
- The `validate()` arm at `src/config.rs:2257` and the vision borrow-check lose their `synthesize.is_none()` cases; `warn_on_inert_settings` drops both earned arms (keep the fn if other warnings remain, else delete it and its call).
- `[promote]`: delete `resynthesize_after_unconfirmed` (struct field, default fn, example file mention); keep `activation_above`.
- `Core`: fields out, `synthesizer: Arc<dyn Synthesizer>`, delete `synthesizes()` and mend its call sites (`grep -rn "synthesizes()" src`); `src/jobs/mod.rs`'s class-gating for `Some("synthesize")` units simplifies (the gate can no longer be closed — read the gating code and remove the closed branch).
- `promote::maybe_promote`: gate becomes provenance/state checks only. Delete `maybe_resynthesize` and its call site (grep).
- `segment_budget(core)` → `segment_tokens(core.synthesizer.budget(), prompt_overhead(core))`, no match.
- Test support: fake synthesizer always present; `grep -rn "SynthesisMode" src tests` and mend every site — off/eager-specific tests either assert the new unified behavior or die with the mode.

- [ ] **Step 4: Run** `cargo test` green.
- [ ] **Step 5: Commit** — `feat(config): one mode — a synthesizer is required, SynthesisMode retires`

Also in this commit: `config.example.toml` loses `synthesis`, `segment_tokens`, `resynthesize_after_unconfirmed`, and the three-mode prose (rewrite the `[infer]` header comment to: capture is verbatim-first; a paste that fits one synthesis call is synthesized at capture and supersedes its passages; larger corpora earn synthesis through use).

---

### Task 4: The size fork — synthesis armed at capture

**Files:**
- Modify: `src/jobs/passages.rs` (`capture_verbatim` arms the window when the corpus is one window)
- Modify: `src/jobs/synthesize.rs` (`plan` is now `capture_verbatim` only)
- Test: in `src/jobs/passages.rs` and `src/core/ingest.rs` test mods

**Interfaces:**
- Consumes: `reset_segment(corpus_id, idx, keep_artifacts=true)`, `rearm_idle_seq(Stage::SegmentWindow, "segment", unit_target(..), idx)` — both exist (`src/jobs/promote.rs` uses exactly this pair).
- Produces: the invariant later tasks build on — a single-window corpus always has a `SegmentWindow` unit armed at capture with `keep_artifacts`, so `src/jobs/window.rs` reaches its existing `keep` branch (inline embed + `supersede_covered`) for small captures.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_small_capture_arms_its_synthesis_at_capture() {
    let core = crate::core::test_support::test_core().await;
    let out = core.ingest("remind me friday to send the invoice", "web", None).await.unwrap();
    assert!(crate::jobs::run_one(&core).await.unwrap()); // the Synthesize plan job
    // Verbatim passages exist and the embed job is armed (searchable first).
    let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
    assert!(rows.iter().any(|c| c.provenance == crate::store::artifacts::Provenance::Passage));
    // And the one window is armed for a model read, keeping its passages.
    let target = crate::jobs::window::unit_target(&out.id, 0);
    assert!(core.store.live_job(crate::store::jobs::Stage::SegmentWindow, &target).await.unwrap());
    assert!(core.store.segment_keeps_artifacts(&out.id, 0).await.unwrap());
}

#[tokio::test]
async fn a_large_capture_arms_no_synthesis_and_waits_for_promotion() {
    let core = crate::core::test_support::test_core().await;
    let big = "paragraph words ".repeat(4000); // far over one window's budget
    let out = core.ingest(&big, "web", None).await.unwrap();
    assert!(crate::jobs::run_one(&core).await.unwrap());
    let segs = core.store.segments_for_corpus(&out.id).await.unwrap();
    assert!(segs.len() > 1, "must be multi-window for this test: {}", segs.len());
    for w in &segs {
        let target = crate::jobs::window::unit_target(&out.id, w.idx);
        assert!(!core.store.live_job(crate::store::jobs::Stage::SegmentWindow, &target).await.unwrap());
    }
}
```

- [ ] **Step 2: Run** — first test fails (no unit armed).

- [ ] **Step 3: Implement** — at the end of `capture_verbatim` (`src/jobs/passages.rs`), after `finish`:

```rust
// The size fork. One window = the whole capture fits one synthesis call:
// arm that call now instead of waiting for use to earn it. `keep_artifacts`
// puts the window job on its promotion path — append, embed inline,
// supersede the covered passages — so a failed or slow call leaves the
// verbatim capture searchable and the job retryable.
if windows.len() == 1 {
    core.store.reset_segment(corpus_id, 0, true).await?;
    core.store
        .rearm_idle_seq(
            Stage::SegmentWindow,
            "segment",
            &crate::jobs::window::unit_target(corpus_id, 0),
            0,
        )
        .await?;
    tracing::info!(corpus_id, "small capture: synthesis armed at capture");
}
```

Check `reset_segment`'s semantics in `src/store/segments.rs:281` first: it must set the state back to pending and record `keep_artifacts` — the same call promotion makes. Note `finish` ran before this and set the corpus `Embedding`; the settle after the window completes runs `finish` again (idempotent — same as promotion). If `reset_segment` before `finish` orders better (segment pending → corpus stays `segmenting` until the model answers), prefer arming **after** `finish` exactly as shown: the spec wants the capture searchable before the model answers, and `finish` arming the embed first is that.

- [ ] **Step 4: Run** `cargo test` — both new tests and the promotion suite green (the promotion tests in `src/jobs/promote.rs` use multi-passage fixtures; if any used a one-window corpus and now sees capture-time arming, adjust the fixture to multi-window).

- [ ] **Step 5: Commit** — `feat(capture): the size fork — a one-window capture synthesizes now, supersedes its passages`

---

### Task 5: The judging reply — prompt, schema, parse

**Files:**
- Modify: `src/infer/prompt.rs` (SYNTHESIZER_SYSTEM rewrite; `RawArtifact` gains `tags`/`pinned`; new envelope fields + `Judgement`; delete `REMIND_SYSTEM`, `remind_prompt`, `remind_schema`, `Remind`, `parse_remind`)
- Modify: `src/infer/mod.rs` (`ProposedArtifact` gains `pinned: bool`; `SegmentInput` gains `judge: Option<JudgeAsk<'a>>`; `Synthesizer::segment` returns `SegmentReply`)
- Modify: `src/infer/openai.rs` (`segment` builds the judge block and parses the envelope)
- Modify: `src/infer/fake.rs` (5 fake impls follow the new return type)
- Modify: `src/jobs/window.rs`, `src/jobs/pursuit.rs`(?) — every `synth.segment(...)` caller (grep) unwraps `.artifacts`

**Interfaces:**
- Consumes: nothing new.
- Produces:

```rust
pub struct JudgeAsk<'a> {
    pub now_local: &'a str,          // "2026-09-04 09:00 (Friday)"
    pub tz: &'a str,                 // "Europe/Berlin"
    pub forced_intent: Option<&'a str>, // door said "remind"/"journal"
    pub neighbors: &'a [Neighbor],   // shown, id-addressable
}
pub struct Neighbor { pub id: String, pub title: Option<String>, pub text: String }
pub struct ProposedLink { pub artifact_id: String, pub reason: String }
pub struct Judgement {
    pub intent: Option<String>,      // "remind" | "journal" | "none"
    pub when: Option<String>,        // local ISO-8601, no zone
    pub rule: Option<String>,        // RRULE
    pub events: Vec<String>,         // local ISO-8601 datetimes
    pub links: Vec<ProposedLink>,
}
pub struct SegmentReply { pub artifacts: Vec<ProposedArtifact>, pub judgement: Option<Judgement> }
// Synthesizer::segment(&self, input: SegmentInput<'_>) -> Result<SegmentReply>
```

- [ ] **Step 1: Write the failing parse tests** (in `src/infer/prompt.rs` tests)

```rust
#[test]
fn a_judged_reply_parses_moment_events_links_and_pinned() {
    let body = r#"{"moment":{"intent":"remind","when":"2026-09-04T09:00","rule":null},
        "events":["2026-09-12T00:00"],
        "links":[{"artifact_id":"a-1","reason":"same migration"}],
        "artifacts":[{"text":"Send the invoice","title":"Invoice","category":"other",
                      "corpus_lines":[1,1],"caveats":[],"tags":["billing"],"pinned":true}]}"#;
    let r = parse_judged_response(body).unwrap();
    let j = r.judgement.unwrap();
    assert_eq!(j.intent.as_deref(), Some("remind"));
    assert_eq!(j.when.as_deref(), Some("2026-09-04T09:00"));
    assert_eq!(j.events, vec!["2026-09-12T00:00"]);
    assert_eq!(j.links[0].artifact_id, "a-1");
    assert!(r.artifacts[0].pinned);
    assert_eq!(r.artifacts[0].tags, vec!["billing"]);
}

#[test]
fn a_missing_or_malformed_judgement_never_fails_the_artifacts() {
    let plain = r#"{"artifacts":[{"text":"x","title":null,"category":null,"corpus_lines":[1,1],"caveats":[]}]}"#;
    let r = parse_judged_response(plain).unwrap();
    assert!(r.judgement.is_none() || r.judgement.as_ref().unwrap().intent.is_none());
    assert_eq!(r.artifacts.len(), 1);
    let bad = r#"{"moment":"not an object","artifacts":[{"text":"x"}]}"#;
    assert_eq!(parse_judged_response(bad).unwrap().artifacts.len(), 1);
}
```

- [ ] **Step 2: Run** — fails to compile (`parse_judged_response` absent).

- [ ] **Step 3: Implement**

- `RawArtifact` += `#[serde(default)] tags: Vec<String>`, `#[serde(default)] pinned: bool`; thread both into `ProposedArtifact` where raw→proposed conversion happens (find it next to `parse_response`). Salvage keeps working — it deserializes `RawArtifact` per object.
- New envelope: keep `parse_response` (artifacts only — the promotion path) delegating to a widened internal parse; add:

```rust
#[derive(serde::Deserialize)]
struct JudgedEnvelope {
    #[serde(default, deserialize_with = "lenient")] moment: Option<RawMoment>,
    #[serde(default, deserialize_with = "lenient")] events: Option<Vec<String>>,
    #[serde(default, deserialize_with = "lenient")] links: Option<Vec<RawLink>>,
    artifacts: Vec<RawArtifact>,
}
```

  where `lenient` is a small `Deserialize` helper that turns a type mismatch into `None` instead of an error (the "malformed judgement never fails the artifacts" rule). Salvage path: judgement is `None` when the envelope needed salvaging.
- `SYNTHESIZER_SYSTEM`: rewrite around the embedder. Keep verbatim-literals, context-only, and markdown rules word-for-word where they stand; change the shaping rules to: one idea per artifact; the artifact must stand alone as a *search result* — front-load the terms someone would search for; respect the stated token cap (it already arrives via `max_artifact_tokens`); output `tags` (0–3 short lowercase topic words) and `pinned` (true only for a decision or commitment the operator made — default false). Append the judgement contract (only emitted when the prompt carries a JUDGE block): `moment.intent` one of remind/journal/none — remind only when the note asks future-self to act, journal only when it records what happened today; `when`/`rule` with the exact wording lifted from the deleted `REMIND_SYSTEM` (minutes arithmetic, 09:00 default, never invent a date, RRULE fields); `events` for dates the note states that are not the reminder; `links` only ids shown in the NEIGHBORS block, with a one-line reason.
- `user_prompt`: when `judge` is `Some`, append after the INPUT block:

```text
----- NEIGHBORS (context only; link targets) -----
[id: a-1] Invoice workflow
<neighbor text…>
----- END NEIGHBORS -----
----- JUDGE -----
Current local time: 2026-09-04 09:00 (Friday)
Time zone: Europe/Berlin
The capture door says this is: remind      # only when forced
Judge this note: moment, events, links.
----- END JUDGE -----
```

- `Synthesizer` trait + `HttpSynthesizer::segment` + the 5 fakes + callers: mechanical (`SegmentReply`). The response-format schema (`Some("artifacts")` in `openai.rs`) must widen to allow the new optional top-level fields — find where that named format maps to a JSON schema and add them as optional.
- Delete the remind prompt family; `cargo check` will list the `jobs/moments.rs` callers — leave them broken here only if Task 6 lands in the same session, otherwise stub the calls out compiling; prefer doing Tasks 5–7 in one continuous run.

- [ ] **Step 4: Run** `cargo test` green.
- [ ] **Step 5: Commit** — `feat(synthesis): the reply judges — moment, events, links, tags, pinned; prompt targets the embedder`

---

### Task 6: The window job acts on the judgement

**Files:**
- Create: `src/jobs/judgement.rs` (what the reply's judgement becomes: moments, journal, links)
- Modify: `src/jobs/window.rs` (build `JudgeAsk` + neighbors for the single-window case; call `judgement::apply` after artifacts are written)
- Modify: `src/infer/context.rs` (`ContextBudget` gains `neighbors: usize`; count it in `total()`)
- Modify: `src/config.rs` (context budget key `[infer.synthesize] neighbor_tokens`, default 1024 — follow how `opening`/`overlap` are configured today, grep `ContextBudget` construction)
- Modify: `src/store/links.rs` (new `relate_synthesized(a, b, reason)`)
- Move: `parse_local`, `confirm_created`, `JOURNALABLE` from `src/jobs/moments.rs` into `src/jobs/judgement.rs` (verbatim)

**Interfaces:**
- Consumes: `SegmentReply`/`Judgement`/`JudgeAsk`/`Neighbor` (Task 5); `core.vectors.neighbours(artifact_id, limit)` (`src/vector/mod.rs:351`); `core.store.insert_moment`, `has_moment_at`, `rearm_remind`, `delete_read_moments` (`src/store/moments.rs`); `core.set_entry` (`src/core/ingest.rs:1393`); `validate_rule`, `zone`, `default_zone_name`, `intent_refused` (`src/core/moments.rs`).
- Produces: `judgement::apply(core, corpus_id, first_artifact_id, judgement, shown_ids) -> Result<()>`; `Store::relate_synthesized(&self, a: &str, b: &str, reason: &str) -> Result<()>`.

- [ ] **Step 1: Write the failing tests** (in `src/jobs/judgement.rs`; use the fake synthesizer pattern from `src/infer/fake.rs` — one fake returns a fixed `SegmentReply` with a judgement)

```rust
#[tokio::test]
async fn a_remind_judgement_becomes_a_due_moment_and_rearms_the_band() {
    let core = test_core_with_judged_reply(/* intent: remind, when: tomorrow 09:00 */).await;
    let out = core.ingest("remind me tomorrow to send the invoice", "web", None).await.unwrap();
    drain_capture(&core).await; // run_one until idle (helper below)
    let art = &core.store.artifacts_for_corpus(&out.id).await.unwrap()[0];
    let moments = core.store.moments_for_artifact(&art.id).await.unwrap(); // use the real read fn name
    assert!(moments.iter().any(|m| m.kind == crate::store::moments::Kind::Due));
}

#[tokio::test]
async fn a_link_to_an_unshown_id_is_dropped_and_a_shown_one_lands_related() {
    // Seed one artifact ("a-1"-like) in the base, capture a related note whose
    // fake reply links to it AND to "ghost-99"; assert exactly one link row,
    // state Related, carrying the reason.
}

#[tokio::test]
async fn a_refused_intent_stays_refused_across_resynthesis() {
    // metadata.intent_refused = ["remind"] on the corpus; the judged reply says
    // remind; assert no Due moment is written.
}

#[tokio::test]
async fn events_land_on_the_first_artifact() { /* events: one date → Kind::Event row */ }
```

Write them against the real store API — before coding, `grep -n "pub async fn" src/store/moments.rs` for the read fn names and use those.

- [ ] **Step 2: Run** — compile failures.

- [ ] **Step 3: Implement**

`src/jobs/window.rs`, in `run`, for the judging case (`all.len() == 1`):

```rust
let judging = all.len() == 1;
let neighbors: Vec<crate::infer::Neighbor> = if judging {
    neighbor_context(core, corpus_id, idx).await.unwrap_or_default()
} else { vec![] };
let ask = judging.then(|| build_judge_ask(core, &src_corpus, &neighbors)); // now_local/tz/forced from corpus metadata, as jobs/moments.rs did
let reply = synth.segment(SegmentInput { core: &text, context: &ctx, judge: ask.as_ref() }).await;
```

`neighbor_context`: first passage id of this segment (`artifacts_for_segment`, provenance Passage) → `core.vectors.neighbours(&id, 8)` → drop hits whose payload names this corpus → keep the top 5 → `store.artifacts_by_ids` for title+text → truncate each text to `budget.context.neighbors / 5` counter-tokens. Passages may not be embedded yet if the queue raced — `neighbours` on an unembedded id returns empty/err; treat any error as "no neighbors" (log debug), never fail the window.

After `write_segment_artifacts` + supersede (existing code), when `judging`:

```rust
if let Some(j) = reply.judgement {
    let shown: Vec<&str> = neighbors.iter().map(|n| n.id.as_str()).collect();
    if let Err(e) = crate::jobs::judgement::apply(core, corpus_id, &written[0].id, &j, &shown).await {
        tracing::warn!(corpus_id, error = %e, "the judgement could not be applied; the artifacts stand");
    }
}
```

`judgement::apply` (the moved heart of `jobs/moments.rs::run`, minus classifier):
- resolve tz from corpus metadata exactly as `src/jobs/moments.rs:26-38` does (copy the block);
- `delete_read_moments(first_artifact_id)` first (idempotent re-reads);
- events: each parsed `parse_local(when, tz)` → skip if `has_moment_at(.., Kind::Event, Some(at))` → `insert_moment` with `Source::Classified`, `span: None`;
- intent `"journal"`: `if JOURNALABLE.contains(origin) && !intent_refused(&meta, Intent::Journal) { core.set_entry(cid, true).await?; }`;
- intent `"remind"`: honor `intent_refused(.., Intent::Remind)`; `rule` filtered through `validate_rule`; a judged remind with `when: None` and `rule: None` stays a plain capture (same guard as today, `src/jobs/moments.rs:118-121`); else insert `Kind::Due` (`Source::Classified`, or `Source::Cue` when the door forced it), `rearm_remind()`, `confirm_created(...)` best-effort;
- links: for each `ProposedLink` whose `artifact_id` is in `shown`, `store.relate_synthesized(first_artifact_id, &l.artifact_id, &l.reason)` — and record `intent_read`/`intent_by = "synthesis"` on the corpus metadata the way `record_intent` did (copy that fn in, minus `score`).

`Store::relate_synthesized` (`src/store/links.rs`), modeled on the insert at line 354:

```rust
/// A relation the synthesis call named at capture. Lands `related` with its
/// reason — the judged state, not the learning one — because a model
/// explicitly asserted it over both texts; the link judge never re-asks.
pub async fn relate_synthesized(&self, a: &str, b: &str, reason: &str) -> Result<()> {
    let (a, b) = canonical(a, b);
    sqlx::query(
        "INSERT INTO artifact_links (a_id, b_id, weight, bumped_at, queries, cues, state, reason, created_at)
         VALUES (?, ?, ?, ?, 0, '[]', 'related', ?, ?)
         ON CONFLICT(a_id, b_id) DO UPDATE SET
           state = CASE WHEN state = 'dismissed' THEN state ELSE 'related' END,
           reason = CASE WHEN state = 'dismissed' THEN reason ELSE excluded.reason END",
    )
    .bind(a).bind(b).bind(1.0f64).bind(now()).bind(reason).bind(now())
    .execute(&self.pool).await?;
    Ok(())
}
```

(Check the table's real column set/conflict target against the schema first — `grep -n "artifact_links" src/store/schema*` or the migration files — and keep an operator's `dismissed` unbeatable.)

`ContextBudget`: add `pub neighbors: usize`; `total()` adds it (plus its fence line inside the existing `FENCE_TOKENS` allowance — bump that constant to cover the two new fences). Config default `neighbor_tokens = 1024`, wired wherever `opening`/`overlap` are read from `[infer.synthesize]`.

- [ ] **Step 4: Run** `cargo test` green.
- [ ] **Step 5: Commit** — `feat(judgement): the synthesis call sets the reminder, files the entry, dates the events, names the links`

---

### Task 7: The classifier, the cues, and the date rules die

**Files:**
- Delete: `src/jobs/moments.rs` (whatever Task 6 didn't move)
- Modify: `src/core/moments.rs` (keep: `Intent`, `intent_refused`/`refuse_intent`/`allow_intent`, `zone`, `default_zone_name`, `validate_rule`, `examples_for` re-pointed at static strings, `DEFAULT_HOUR` if the due band reads it — grep. Delete: `PROTOTYPES`, `classify`, `nearest`, `Protos`, `cue`, `weak_cue`, `Strength`, `absolute_dates`, `relative_date`, `clock_offset`, `Found`, month-first parsing, and their tests)
- Modify: `src/core/mod.rs` (delete `protos` field, `prototypes()`, `reminder` field and its build)
- Modify: `src/store/jobs.rs` (delete `Stage::Moments`: enum arm, `as_str`, `parse`, the class lists at `:109/:133/:170/:195`, the `class = 0 OR stage = 'moments'` query at `:783` — read its comment and mend the query)
- Modify: `src/jobs/mod.rs` (delete the `Stage::Moments` dispatch at `:183`)
- Modify: `src/jobs/embed.rs` (whatever arms `Stage::Moments` after an embed — grep `Moments` — delete the arming)
- Modify: `src/core/ingest.rs:307-331` (`ingest_capture`: the cue-based journal filing goes; a **forced** `intent == "journal"` on a journalable origin keeps the origin rewrite; everything else waits for the judgement)
- Modify: `src/web/workspace.rs:236` (`examples_for` now returns static examples — keep the call shape)
- Modify: `src/web/ui.rs:530-560` (`intent_echo` — Task 8 rewrites it; here just make it compile by removing dead imports if Task 8 is not in the same run; prefer same run)

**Interfaces:**
- Consumes: Task 6's `judgement::apply` being the only intent writer.
- Produces: `moments::examples_for(accept_language) -> (&'static str, &'static str)` (same signature, static table).

- [ ] **Step 1: Write the one new test** — forced journal still files at the door:

```rust
#[tokio::test]
async fn a_forced_journal_is_filed_at_the_door_without_a_cue_table() {
    let core = crate::core::test_support::test_core().await;
    let mut c = crate::core::ingest::Capture::new("Heute den Bericht abgegeben", "web");
    c.metadata["intent"] = serde_json::Value::String("journal".into());
    let out = core.ingest_capture(c).await.unwrap();
    let src = core.store.get_corpus(&out.id).await.unwrap();
    assert_eq!(src.origin, crate::core::ingest::ORIGIN_JOURNAL);
}
```

- [ ] **Step 2: Delete top-down** — remove `Stage::Moments` first, follow `cargo check` errors through the store, jobs, core, and web layers; the compiler is the checklist. `grep -rn "moments::\|Stage::Moments\|prototypes\|weak_cue\|absolute_dates\|relative_date\|core.reminder\|\.protos" src tests` must end empty except `store/moments.rs` (the store survives) and the keepers listed above.

- [ ] **Step 3: Static examples** — `examples_for` keeps its language switch, returns fixed phrasings (one remind, one journal per language, reuse the first prototype strings verbatim as literals — they were chosen to read well).

- [ ] **Step 4: Run** `cargo test`; delete the classifier's test corpus wholesale (tests named for cues/classify/dates in `core/moments.rs` and `jobs/moments.rs`), keep and re-home the `validate_rule` and tz tests.

- [ ] **Step 5: Commit** — `feat(moments): the classifier, cues and date rules retire; the synthesis judgement is the reader`

---

### Task 8: The fate echo — the box says what capture will do

**Files:**
- Modify: `src/web/ui.rs` (`intent_echo` → `fate_echo`; same template slot, same search-response ride at `:1380` and `:696`)
- Modify: `src/web/templates/_intent_echo.html` (rename content; keep the `id="intent-echo"` slot and oob swap)
- Modify: `src/web/templates/_box_hint.html` (comment references the classifier — update prose)

**Interfaces:**
- Consumes: `core.counter`, `crate::jobs::synthesize::segment_budget(core)` — make `segment_budget` `pub(crate)`→`pub` if the web layer needs it (it is `pub(crate)` today, same crate — fine).
- Produces: `fate_echo(core: &Core, q: &str) -> IntentEchoTemplate`-shaped fn; template fields `{ kind: &'static str, detail: String }`.

**Spec deviation, deliberate:** the spec names `POST /ui/capture/probe`; the codebase already sends a search request per keystroke on a 120ms debounce and rides the echo on its response (`src/web/ui.rs:525` comment). The fate echo rides the same response — identical UX, one fewer endpoint. Note it in the spec file (one line in §5) in this task's commit.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn a_small_paste_echoes_synthesis_and_a_large_one_echoes_its_windows() {
    // build a test core; budget = segment_budget(&core)
    let small = fate_echo(&core, "remind me friday");
    assert_eq!(small.kind, "will be synthesized");
    let big_text = "words ".repeat(50_000);
    let big = fate_echo(&core, &big_text);
    assert_eq!(big.kind, "large paste");
    assert!(big.detail.contains("verbatim"));
    let empty = fate_echo(&core, "");
    assert_eq!(empty.kind, "");
}
```

- [ ] **Step 2: Implement**

```rust
/// What capture will do with the box, said before it is pressed. Pure local
/// arithmetic on the same counter and budget the fork uses — exact, no model,
/// no store read — riding the search response the box already makes.
pub(crate) fn fate_echo(core: &Core, q: &str) -> IntentEchoTemplate {
    if q.trim().is_empty() {
        return IntentEchoTemplate { kind: "", detail: String::new() };
    }
    let budget = crate::jobs::synthesize::segment_budget(core);
    let tokens = core.counter.count(q);
    if tokens <= budget {
        IntentEchoTemplate {
            kind: "will be synthesized",
            detail: "captured verbatim, then rewritten into structured artifacts".into(),
        }
    } else {
        let windows = tokens.div_ceil(budget);
        IntentEchoTemplate {
            kind: "large paste",
            detail: format!("stored verbatim in ~{windows} windows; synthesis comes with use"),
        }
    }
}
```

Template: `<span class="intent-echo-kind">{{ kind }}</span> · {{ detail }}` under the existing empty-swap rule. Replace both call sites of `intent_echo` (`:696`, `:1380`); delete `intent_echo`.

- [ ] **Step 3: Run** `cargo test` (ui tests referencing the echo — grep `intent_echo` in tests and mend).
- [ ] **Step 4: Commit** — `feat(capture): the box says its fate — synthesized, or stored verbatim in N windows` (include the one-line spec note).

---

### Task 9: The judge page dies; tune-apply moves to insights

**Files:**
- Delete: `src/web/judge.rs`; templates `judge.html`, `_judge_card.html`, `_judge_full.html`, `_judge_pulse.html`, `_judge_assign.html`, `_judge_assign_results.html`
- Move: `_judge_tune.html` → `_tune.html`; `tune_apply` + `tune_fragment` handlers → `src/web/insights.rs` at route `/ui/insights/tune/{run_id}/apply` (same `CanJudge` gate)
- Move: `pub fn ago` (used by `src/web/ui.rs:684`) → `src/web/ui.rs`
- Modify: `src/web/mod.rs:101` (unmerge), `src/web/state.rs:113` (`judge_pending` deleted), `src/web/templates/layout.html:64-73,115-121` (nav entries out), `src/web/ui.rs:715-720,818-819` (template fields out), `src/web/insights.rs` (render the open recommendation + `_tune.html` beside the existing `Retrieval` block; delete its own `judge_pending` field), `src/web/templates/insights.html:65` ("Review some" link → "Answer *Was this what you were looking for?* under results and asks, and the number appears.")
- Modify: `src/store/feedback.rs` — delete the deck-only surface (`next card` dealing, `DEALT`-pool readers) **only after** `cargo check` proves nothing else calls each fn; inline verdict paths (`open_event`, verdict writes, `feedback_stats`) stay.

**Interfaces:**
- Consumes: `CanJudge` (`src/web/tenant.rs`), `store.eval_run`, `store.open_recommendation` (already used by `tune_apply`), `feedback_stats` (already on insights, `src/web/insights.rs:540`).
- Produces: `/ui/judge*` answers 404; tune-apply lives at `/ui/insights/tune/{run_id}/apply`.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn every_judge_route_is_gone() {
    // reuse the web test harness from src/web/judge.rs's own tests (steal its
    // setup before deleting the file)
    for path in ["/ui/judge", "/ui/judge/next"] {
        let res = get(path).await;
        assert_eq!(res.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn tune_apply_answers_on_insights_behind_the_grant() {
    // an ungranted user is refused; a granted one with an open recommendation
    // applies it — port the existing tune_apply test from judge.rs.
}
```

- [ ] **Step 2: Move first, delete second** — port `tune_apply`/`tune_fragment`/`ago` and their tests, get green, then delete `judge.rs` + templates and follow `cargo check`. Also `grep -rn "ui/judge\|judge_pending\|judge::" src tests docs` until only history remains.

- [ ] **Step 3: Run** the full suite; `tests/eval.rs:175` mentions judging in a message string — update the wording (it tells the operator where to judge; the answer is now "under each result").

- [ ] **Step 4: Commit** — `feat(judge): the deck retires; inline verdicts are the labeller, tune-apply moves to insights`

---

### Task 10: Docs — README, config example, evaluation

**Files:**
- Modify: `README.md` (the `infer.synthesis` paragraph → the size-fork story; the "Judge" bullet → inline verdicts + insights; the no-model claim in "What it does" adjusted — a chat model is required, PDFs still read locally)
- Modify: `config.example.toml` (final pass: `[infer]` header prose, `tokenizer` key documented, `neighbor_tokens` documented, `[promote]` comment loses the eager paragraph)
- Modify: `docs/evaluation.md` (deck references → inline judging; the position-bias paragraph about "five, in order" moves or is trimmed to what inline judging still does)

**Steps:**
- [ ] **Step 1:** Rewrite each file against the spec's §1/§5/§7 — every claim in the README must be true of the code as of Task 9 (re-read the shipped behavior, not the old text).
- [ ] **Step 2:** `grep -rn "earned\|eager\|synthesis =" README.md config.example.toml docs/evaluation.md` — no survivors except history notes.
- [ ] **Step 3:** Commit — `docs: the reshape as shipped — one pipeline, a judged capture, no deck`

---

### Task 11: End-to-end verification

**Steps:**
- [ ] **Step 1:** `cargo test` and `cargo clippy --all-targets` — both clean.
- [ ] **Step 2:** Fresh instance (the operator's recipe: podman qdrant + TEI + minimal config — plus a reachable `[infer.synthesize]` endpoint, now mandatory; ask the operator for one if none is running). Capture through the web box:
  1. `remind me friday 16:00 to send the invoice` — expect: fate echo says *will be synthesized*; capture lands searchable at once; within the model's time a Due moment appears on the due band; the artifact is a structured rewrite superseding the passage (lineage shows the original).
  2. a paragraph of unstructured notes mentioning a date — expect: an Event on the day page, tags on the artifact, a link if a neighbor exists.
  3. a large pasted manual — expect: fate echo says *large paste*; verbatim passages searchable; no synthesis call; opening one passage repeatedly past `activation_above` promotes its window (existing behavior).
  4. `/ui/judge` → 404; insights shows retrieval numbers and (after a sweep) the tune block.
- [ ] **Step 3:** Report what was seen, including anything that did not match — this list is the acceptance test for the spec.

---

## Self-review (performed at plan-writing time)

- **Spec coverage:** §1→Task 3; §2→Tasks 2+4; §3→Task 1; §4→Task 2; §5→Tasks 5+6+8; §6→Tasks 6+7; §7→Task 9; §8→every task's tests + Task 11. Gap: none found.
- **Known deviation:** §5's probe endpoint is implemented as the fate echo riding the existing per-keystroke search response (Task 8) — same UX, fewer parts; the task updates the spec line.
- **Type consistency:** `SegmentReply`/`Judgement`/`JudgeAsk` defined in Task 5, consumed by Task 6/8 under the same names; `TokenCounter::load/default` (Task 1) used in Tasks 2/8; `Window` without `carry_lines` (Task 2) consumed by Task 4's fork.
