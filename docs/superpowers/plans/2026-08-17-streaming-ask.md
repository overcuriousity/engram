# Streaming Ask, the Retrieval Loop and Model Tiers — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the config two named model tiers; make `ask` pack excerpts to the relevance cliff, reach one hop sideways for candidates, and check its own answer for invented literals; stream the completion to the page with citations as links; and add one bounded extra retrieval round plus a button that keeps an answer.

**Architecture:** Tiers are resolved **at config parse time** into the existing `SynthesizeRole` / `AskRole` / `VisionRole` structs, so `infer/openai.rs` and every consumer are untouched. `src/core/ask.rs` becomes a module directory; `Core::ask_events` is the producer and `Core::ask` becomes a collector over it, so `/api/v1/ask` and MCP keep one implementation and stay non-streaming. Streaming reaches the endpoint through a **defaulted** `Completer::answer_streaming`, so every existing implementor and every existing test keeps working.

**Tech Stack:** Rust 2024, axum 0.8 (SSE), askama 0.16, sqlx 0.9 (SQLite), tokio, reqwest 0.13 (+`stream` feature), serde_json, `async-stream`. Tests: `cargo test`, plus `cargo test --test eval -- --ignored` for the harness.

**Spec:** `docs/superpowers/specs/2026-08-17-streaming-ask-design.md`

## Global Constraints

- Branch: create `feat/streaming-ask` off `master` before Task 1. Commit after every task; `cargo fmt`, `cargo clippy --all-targets` and `cargo test` must be clean before each commit.
- **No inference on the search path.** Ask is the one door that generates at read time. The follow-up round is the only new model call in this plan, it is bounded to exactly one, and it ships **off** (`follow_up = false`).
- **An answer cannot carry a literal the excerpts did not.** Task 7's check is the enforcement; it is a string operation and costs no call.
- **A default moves only after the harness has run.** No ranking or packing default changes without a harness run behind it. `follow_up` defaults off for this reason.
- Neighbours are **appended after** the ranked hits, never interleaved, and never enter the scores passed to `search::cliff`.
- Constants, exact values: neighbour anchors `NEIGHBOUR_ANCHORS = 3`, neighbour cap `NEIGHBOUR_MAX = 6`, one-shot ask id TTL `ASK_HANDOFF_TTL = Duration::from_secs(60)`.
- Phase 1 is a refactor: **every existing test must pass with no edit.** That is its acceptance criterion.
- House style: doc comments say *why*, not *what*. Test names are sentences (`a_list_with_a_cliff_packs_to_it`). Errors are `crate::error::Error`. Ids from `crate::store::new_id()`, timestamps from `crate::store::now()`.
- Existing helpers to reuse, never reinvent: `crate::core::search::cliff`, `crate::infer::verify::missing_literals`, `crate::infer::budget::pack_by_budget`, `crate::store::links::Store::links_from`, `crate::web::markdown::render`, `crate::web::test_support::{app_with_cookie, body_of}`.

---

## File map

| File | Responsibility |
|---|---|
| `src/config.rs` | `TierConfig`, `RawInferConfig` + per-role raw structs, `TryFrom<RawInferConfig> for InferConfig`, legacy-shape warnings emitted in `normalize` |
| `config.example.toml` | `[infer.tiers.*]` blocks; roles rewritten to `tier = "…"` |
| `src/core/ask/mod.rs` | moved from `src/core/ask.rs`; `AskEvent`, `ask_events`, `ask` as collector, `AskResponse::unsupported` |
| `src/core/ask/retrieve.rs` (new) | `packed_count`, `anchor_count`, `append_neighbours`, `NEIGHBOUR_ANCHORS`, `NEIGHBOUR_MAX` |
| `src/core/ask/check.rs` (new) | `unsupported_literals`, `mark_unsupported` |
| `src/core/ask/stream.rs` (new) | `AskEvent` |
| `src/core/ask/follow_up.rs` (new) | `needed_query` — the one extra round |
| `src/store/artifacts.rs` | `Store::adjacent_artifacts` |
| `src/infer/mod.rs` | `Delta`, `Completer::answer_streaming` (defaulted) |
| `src/infer/openai.rs` | `HttpCompleter::answer_streaming` (SSE), `HttpCompleter::for_follow_up` |
| `src/infer/prompt.rs` | `FOLLOW_UP_SYSTEM`, `follow_up_prompt`, `follow_up_schema`, `parse_follow_up` |
| `src/core/mod.rs` | `Core::follow_up: Option<Arc<dyn Completer>>` |
| `src/web/state.rs` | `AppState::ask_handoff` one-shot map |
| `src/web/ui.rs` | `POST /ui/ask` → handoff id, `GET /ui/ask/{id}/stream`, `POST /ui/ask/{id}/capture`, capture `?text=` prefill |
| `src/web/templates/ask.html`, `_answer.html` | live region, rail, citation links, capture button |
| `assets/app.js`, `assets/app.css` | EventSource driver, `.cite`, `.unsupported`, reasoning block |
| `src/eval/metrics.rs`, `tests/eval.rs` | unsupported-literal count in `evaluate_ask` |
| `Cargo.toml` | `async-stream`; reqwest `stream` feature |
| `ROADMAP.md`, `README.md` | mark items done, document tiers |

---

# Phase 1 — Model tiers

### Task 1: Tiers resolved at parse time

The insight that makes this small: `HttpCompleter::new` takes `&AskRole`, `for_judging` takes `&SynthesizeRole`, and so on. If tiers are resolved **during deserialization** into those same structs with every field concrete, nothing downstream changes at all.

**Files:**
- Modify: `src/config.rs` (add types near `InferConfig` at line 361; warning emission in `normalize` at line 817)
- Modify: `config.example.toml:50-180`
- Test: `src/config.rs` (in the existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct TierConfig`; `InferConfig` unchanged in shape, now built via `TryFrom<RawInferConfig>`; `InferConfig::legacy_warnings: Vec<String>`.

- [ ] **Step 1: Write the failing test**

Add to `src/config.rs` tests:

```rust
/// The whole point of the rename: a role that names a tier and a role that
/// carries the same endpoint inline must produce the same completer settings.
/// If these ever diverge, an operator's migration silently changes their model.
#[test]
fn a_tier_and_an_inline_endpoint_resolve_to_the_same_role() {
    let tiered: Config = toml::from_str(
        r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficient"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
    )
    .expect("tiered config parses");

    let inline: Config = toml::from_str(
        r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.synthesize]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        "#,
    )
    .expect("the legacy shape still parses");

    assert_eq!(tiered.infer.synthesize.base_url, inline.infer.synthesize.base_url);
    assert_eq!(tiered.infer.synthesize.model, inline.infer.synthesize.model);
    assert_eq!(
        tiered.infer.synthesize.context_tokens,
        inline.infer.synthesize.context_tokens
    );
    assert_eq!(
        tiered.infer.synthesize.max_output_tokens,
        inline.infer.synthesize.max_output_tokens
    );
    assert_eq!(tiered.infer.ask.base_url, inline.infer.ask.base_url);
    assert_eq!(tiered.infer.ask.model, inline.infer.ask.model);
}

/// A role may override any field its tier defines. Without this the two tiers
/// would have to multiply by every ceiling an operator wants.
#[test]
fn a_role_field_overrides_the_tier_it_points_at() {
    let cfg: Config = toml::from_str(
        r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.deep]
        base_url = "http://localhost:8000/v1"
        model = "big"
        context_tokens = 131072
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "deep"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "deep"
        max_output_tokens = 4096
        "#,
    )
    .unwrap();
    assert_eq!(cfg.infer.ask.max_output_tokens, 4096, "the role's value wins");
    assert_eq!(cfg.infer.ask.context_tokens, 131072, "unset fields come from the tier");
    assert_eq!(cfg.infer.synthesize.max_output_tokens, 16384);
}

/// A typo in a tier name must be a startup failure naming the typo, never a
/// silent fallback to some other model.
#[test]
fn a_role_pointing_at_a_tier_that_does_not_exist_is_refused() {
    let err = toml::from_str::<Config>(
        r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.tiers.efficient]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        [infer.synthesize]
        tier = "efficent"
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        tier = "efficient"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("efficent"), "the error must name the typo: {err}");
    assert!(err.contains("efficient"), "and what was available: {err}");
}

/// The legacy shape is accepted, but never silently: an operator must be told
/// what to write instead. Same reasoning as `SynthesizeRole::cooldown_secs`.
#[test]
fn the_legacy_shape_records_a_warning_naming_its_replacement() {
    let cfg: Config = toml::from_str(
        r#"
        [server]
        bind = "127.0.0.1:8080"
        [store]
        path = "x.db"
        [vector]
        url = "http://localhost:6333"
        collection = "engram"
        [infer.synthesize]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        output_ratio = 8.0
        [infer.embed]
        base_url = "http://localhost:8000/v1"
        model = "bge-m3"
        dim = 1024
        max_input_tokens = 1024
        [infer.ask]
        base_url = "http://localhost:8000/v1"
        model = "qwen"
        context_tokens = 32768
        max_output_tokens = 16384
        "#,
    )
    .unwrap();
    assert_eq!(cfg.infer.legacy_warnings.len(), 2, "one per inline role");
    assert!(cfg.infer.legacy_warnings.iter().any(|w| w.contains("infer.synthesize")));
    assert!(cfg.infer.legacy_warnings.iter().any(|w| w.contains("infer.tiers")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests -- --nocapture`
Expected: FAIL — `unknown field 'tier'`, and `legacy_warnings` does not exist.

- [ ] **Step 3: Implement the types and the resolution**

In `src/config.rs`, replace the `InferConfig` derive at line 361 and add below it:

```rust
/// A named endpoint and its defaults. Roles point at one instead of each
/// carrying its own, so "which model is this call worth" is a decision made
/// once rather than repeated per role.
#[derive(Debug, Deserialize, Clone)]
pub struct TierConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub ceiling_param: Option<CeilingParam>,
    #[serde(default = "default_true")]
    pub structured_output: bool,
}

/// The resolved roles. Deserialised through `RawInferConfig` so that tiers are
/// flattened away before anything downstream sees a role: `HttpCompleter::new`
/// and friends keep taking a struct whose every field is concrete.
#[derive(Debug, Clone)]
#[serde(try_from = "RawInferConfig")]
pub struct InferConfig {
    pub synthesize: SynthesizeRole,
    pub embed: EmbedRole,
    pub ask: AskRole,
    pub rerank: Option<RerankRole>,
    pub vision: Option<VisionRole>,
    /// Emitted by `normalize`. Collected here rather than logged during
    /// deserialization because a `TryFrom` runs before the subscriber is up.
    pub legacy_warnings: Vec<String>,
}
```

Add `#[derive(Deserialize)]` to `InferConfig`'s attribute list alongside the
`serde(try_from)` (serde requires the derive to generate the impl that defers
to `TryFrom`).

Add the raw shapes:

```rust
#[derive(Debug, Deserialize)]
pub struct RawInferConfig {
    #[serde(default)]
    tiers: HashMap<String, TierConfig>,
    synthesize: RawSynthesizeRole,
    embed: EmbedRole,
    ask: RawAskRole,
    #[serde(default)]
    rerank: Option<RerankRole>,
    #[serde(default)]
    vision: Option<RawVisionRole>,
}

/// Every endpoint field optional: it comes from the tier unless the role
/// overrides it, and in the legacy shape the role carries it directly.
#[derive(Debug, Deserialize)]
struct RawSynthesizeRole {
    #[serde(default)] tier: Option<String>,
    #[serde(default)] base_url: Option<String>,
    #[serde(default)] model: Option<String>,
    #[serde(default)] api_key: Option<String>,
    #[serde(default)] context_tokens: Option<usize>,
    #[serde(default)] max_output_tokens: Option<usize>,
    #[serde(default)] timeout_secs: Option<u64>,
    #[serde(default)] reasoning_effort: Option<String>,
    #[serde(default)] ceiling_param: Option<CeilingParam>,
    #[serde(default)] structured_output: Option<bool>,
    // Role-only, unchanged.
    output_ratio: f32,
    #[serde(default)] tokenizer_path: Option<String>,
    #[serde(default)] cooldown_secs: Option<u64>,
    #[serde(default = "default_context_opening_tokens")] context_opening_tokens: usize,
    #[serde(default = "default_context_overlap_tokens")] context_overlap_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct RawAskRole {
    #[serde(default)] tier: Option<String>,
    #[serde(default)] base_url: Option<String>,
    #[serde(default)] model: Option<String>,
    #[serde(default)] api_key: Option<String>,
    #[serde(default)] context_tokens: Option<usize>,
    #[serde(default)] max_output_tokens: Option<usize>,
    #[serde(default)] timeout_secs: Option<u64>,
    #[serde(default)] reasoning_effort: Option<String>,
    #[serde(default)] ceiling_param: Option<CeilingParam>,
    // Role-only. Task 12 reads these; declared here so the shape is settled once.
    #[serde(default)] follow_up: bool,
    #[serde(default)] follow_up_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawVisionRole {
    model: String,
    #[serde(default)] tier: Option<String>,
    #[serde(default)] base_url: Option<String>,
    #[serde(default)] api_key: Option<String>,
    #[serde(default)] timeout_secs: Option<u64>,
    #[serde(default)] max_output_tokens: Option<usize>,
    #[serde(default)] ceiling_param: Option<CeilingParam>,
}
```

Add the two fields to the resolved `AskRole` (§4 of the spec needs them, and
declaring them now means Task 11 adds no second config migration):

```rust
    /// One bounded extra retrieval round. Off by default: it costs a call, and
    /// a default moves only after the harness has run.
    pub follow_up: bool,
    /// The resolved endpoint the "what do I still need" call runs on, from
    /// `follow_up_tier`. `None` falls back to this role's own endpoint.
    ///
    /// A `TierConfig` rather than a role, because that is honestly what it is:
    /// an endpoint and its ceilings, which Task 11 hands straight to a
    /// completer. That call is a cheap classification and belongs on the
    /// efficient model even when the answer it feeds belongs on the deep one —
    /// which is the capability this whole rename exists to express.
    pub follow_up_endpoint: Option<TierConfig>,
```

`RawInferConfig` needs `use std::collections::HashMap;` at the top of the file if
it is not already imported.

Now the resolution. Add near `impl Config`:

```rust
/// Pick a role's value for one field: explicit override, else the tier's, else
/// the error that names what is missing.
fn resolve_endpoint(
    role: &str,
    tier_name: Option<&str>,
    tiers: &HashMap<String, TierConfig>,
    inline_base_url: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<TierConfig, String> {
    if let Some(name) = tier_name {
        return tiers.get(name).cloned().ok_or_else(|| {
            let mut known: Vec<&str> = tiers.keys().map(String::as_str).collect();
            known.sort_unstable();
            format!(
                "[infer.{role}] points at tier `{name}`, which is not defined. \
                 Known tiers: {}. Define it under [infer.tiers.{name}].",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            )
        });
    }
    if inline_base_url.is_some() {
        warnings.push(format!(
            "[infer.{role}] carries its endpoint inline. Move base_url, model, api_key, \
             context_tokens and max_output_tokens into an [infer.tiers.<name>] block and \
             write `tier = \"<name>\"` here. The inline form still works and will be removed."
        ));
        // Caller builds the anonymous tier from the role's own fields.
        return Err(String::new());
    }
    Err(format!(
        "[infer.{role}] has neither `tier` nor `base_url`. Point it at an \
         [infer.tiers.<name>] block."
    ))
}
```

Then `impl TryFrom<RawInferConfig> for InferConfig` with `type Error = String`,
building each role: call `resolve_endpoint`; on the empty-string sentinel build
an anonymous `TierConfig` from the role's inline fields; then fill each resolved
field as `role_override.unwrap_or(tier.field)`.

Finally, in `normalize` (line 817), emit them:

```rust
for w in &self.infer.legacy_warnings {
    tracing::warn!("{w}");
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: PASS, all four.

- [ ] **Step 5: Verify the refactor changed no behaviour**

Run: `cargo test`
Expected: PASS with **zero edits to any pre-existing test**. If a test needed changing, the resolution is wrong — fix the resolution, not the test.

- [ ] **Step 6: Rewrite `config.example.toml`**

Replace the inline endpoints in `[infer.synthesize]` (line 50) and `[infer.ask]` (line 138) with `tier = "efficient"` / `tier = "deep"`, and add the two tier blocks above them. Keep every existing explanatory comment attached to the field it explains — the comments on `output_ratio`, `max_output_tokens` and `ceiling_param` are the documentation and must not be lost in the move. Leave `[infer.embed]` and `[infer.rerank]` exactly as they are.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs config.example.toml
git commit -m "feat(config): named model tiers, resolved at parse time

Roles point at [infer.tiers.<name>] instead of each carrying an endpoint.
Resolution happens during deserialization into the existing role structs,
so infer/openai.rs and every consumer are untouched.

The inline shape still parses and warns, naming its replacement: making
tier required would turn five keys someone chose on purpose into unknown
keys, silently ignored behind a 'missing field tier'."
```

---

# Phase 2 — Retrieval

### Task 2: Split `core/ask.rs` into a module

A pure move, no behaviour change, so the diff for the real work stays readable.

**Files:**
- Create: `src/core/ask/mod.rs` (contents of `src/core/ask.rs`, verbatim)
- Delete: `src/core/ask.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: no API change whatsoever. `crate::core::ask::{AskRequest, AskResponse}` resolve exactly as before.

- [ ] **Step 1: Move the file**

```bash
mkdir -p src/core/ask
git mv src/core/ask.rs src/core/ask/mod.rs
```

- [ ] **Step 2: Verify nothing else needs touching**

Run: `cargo test`
Expected: PASS. `mod ask;` in `src/core/mod.rs` resolves to the directory with no edit. If anything else changed, the move was not pure.

- [ ] **Step 3: Commit**

```bash
git add -A src/core
git commit -m "refactor(ask): move core/ask.rs to core/ask/mod.rs

Pure move. The retrieval loop roughly doubles this file, so the split
lands first and the real diffs stay readable."
```

### Task 3: Pack to the cliff

**Files:**
- Create: `src/core/ask/retrieve.rs`
- Modify: `src/core/ask/mod.rs` (the packing block, currently `pack_by_budget` around line 120-160)
- Test: `src/core/ask/retrieve.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `crate::core::search::cliff(&[f32]) -> Option<usize>`, `crate::infer::budget::pack_by_budget(&[String], &TokenCounter, usize) -> usize`.
- Produces: `pub(super) fn packed_count(scores: &[f32], blocks: &[String], counter: &TokenCounter, budget: usize) -> usize`.

- [ ] **Step 1: Write the failing test**

Create `src/core/ask/retrieve.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::TokenCounter;

    /// Ten-token blocks and a budget that fits all of them, so only the cliff
    /// can do any cutting here.
    fn blocks(n: usize) -> Vec<String> {
        (0..n).map(|_| "word ".repeat(10)).collect()
    }

    /// The whole point: a list whose relevance falls off is cut where it falls
    /// off, not where the context window runs out.
    #[test]
    fn a_list_with_a_cliff_packs_to_it() {
        let scores = [0.9, 0.88, 0.86, 0.20, 0.19];
        assert_eq!(
            packed_count(&scores, &blocks(5), &TokenCounter, 100_000),
            3
        );
    }

    /// No cliff means no basis for concluding anything about the tail, so the
    /// behaviour is exactly what it was before this function existed.
    #[test]
    fn a_list_without_a_cliff_packs_everything_the_budget_allows() {
        let scores = [0.9, 0.88, 0.86, 0.84, 0.82];
        assert_eq!(
            packed_count(&scores, &blocks(5), &TokenCounter, 100_000),
            5
        );
    }

    /// The cliff decides what is worth showing; the window decides what fits,
    /// and the window still wins. An excerpt that does not fit cannot be sent
    /// whatever its relevance.
    #[test]
    fn the_budget_still_wins_when_the_cliff_would_overrun_the_window() {
        let scores = [0.9, 0.88, 0.86, 0.20, 0.19];
        let packed = packed_count(&scores, &blocks(5), &TokenCounter, 25);
        assert!(packed < 3, "the window must cut below the cliff: {packed}");
    }

    /// Fewer than three hits: `cliff` returns None by construction and the
    /// budget is the only bound.
    #[test]
    fn two_hits_are_packed_without_a_cliff() {
        assert_eq!(packed_count(&[0.9, 0.1], &blocks(2), &TokenCounter, 100_000), 2);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib core::ask::retrieve`
Expected: FAIL — `cannot find function packed_count`.

- [ ] **Step 3: Implement**

At the top of `src/core/ask/retrieve.rs`:

```rust
//! Turning a ranked list into the excerpts one answer is built from.

use crate::infer::budget::{TokenCounter, pack_by_budget};

/// How many leading excerpts to send: the cliff, bounded by the window.
///
/// `pack_by_budget` alone is a bound on cost, not on relevance — it will hand
/// the model eight excerpts when the fourth was already noise, and noise makes
/// the answer worse as well as dearer. `search::cliff` is where the ranked list
/// stops meaning anything, and it is the same computation the rail draws.
///
/// The budget bound stays, and stays second: the cliff decides what is worth
/// showing, the window decides what fits, and an excerpt that does not fit
/// cannot be sent whatever its relevance.
///
/// No cliff — fewer than three hits, or no single step standing out — leaves
/// behaviour exactly as it was. A list with no cliff is a list with nothing to
/// conclude from, and inventing a cut there would be worse than the greedy pack.
pub(super) fn packed_count(
    scores: &[f32],
    blocks: &[String],
    counter: &TokenCounter,
    budget: usize,
) -> usize {
    let by_cliff = crate::core::search::cliff(scores).unwrap_or(blocks.len());
    let by_budget = pack_by_budget(blocks, counter, budget);
    by_cliff.min(by_budget)
}
```

Add `mod retrieve;` to `src/core/ask/mod.rs`, and replace the existing
`pack_by_budget` call site with `retrieve::packed_count(&scores, &blocks, &self.counter, budget)`,
where `scores` is `hits.iter().map(|h| h.score).collect::<Vec<_>>()`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib core::ask`
Expected: PASS, including the pre-existing ask tests.

- [ ] **Step 5: Commit**

```bash
git add src/core/ask
git commit -m "feat(ask): pack excerpts to the relevance cliff

pack_by_budget bounds cost, not relevance. search::cliff is where the
ranked list stops meaning anything; the window still wins second."
```

### Task 4: One hop sideways

**Files:**
- Modify: `src/store/artifacts.rs` (add `adjacent_artifacts` near `artifacts_of_corpus`, line ~412)
- Modify: `src/core/ask/retrieve.rs`
- Modify: `src/core/ask/mod.rs`
- Test: both files, inline

**Interfaces:**
- Consumes: `Store::links_from(&[String], &[LinkState], f64, i64, f64, i64) -> Result<Vec<LinkedTo>>`.
- Produces: `Store::adjacent_artifacts(&self, corpus_id: &str, ordinal: i64) -> Result<Vec<Chunk>>`; `pub(super) const NEIGHBOUR_ANCHORS: usize = 3;`, `pub(super) const NEIGHBOUR_MAX: usize = 6;`, `pub(super) fn anchor_count(cliff_at: Option<usize>, hits: usize) -> usize`.

- [ ] **Step 1: Write the failing tests**

In `src/store/artifacts.rs` tests:

```rust
/// The answer is often in the artifact next to the one that matched, and
/// `ordinal` is already a continuous per-corpus sequence, so this is a lookup
/// rather than a search.
#[tokio::test]
async fn adjacent_artifacts_returns_the_ordinals_either_side() {
    let store = test_store().await;
    let corpus = seed_corpus_with_artifacts(&store, 5).await; // ordinals 0..4
    let got = store.adjacent_artifacts(&corpus, 2).await.unwrap();
    let mut ordinals: Vec<i64> = got.iter().map(|c| c.ordinal).collect();
    ordinals.sort_unstable();
    assert_eq!(ordinals, vec![1, 3]);
}

/// The first artifact has no left neighbour, and asking for ordinal -1 must
/// return one row rather than an error or an empty result.
#[tokio::test]
async fn adjacent_artifacts_at_the_edge_returns_only_the_side_that_exists() {
    let store = test_store().await;
    let corpus = seed_corpus_with_artifacts(&store, 5).await;
    let got = store.adjacent_artifacts(&corpus, 0).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].ordinal, 1);
}
```

(Use whatever the file's existing test helpers are named for creating a store
and seeding a corpus; read the neighbouring tests in that `mod tests` and follow
them exactly rather than introducing new helpers.)

In `src/core/ask/retrieve.rs` tests:

```rust
/// Neighbours are reached, not retrieved, so they carry no comparable score.
/// Letting one into the score list would corrupt the cliff computation that
/// just ran — this asserts the ordering that prevents it.
#[test]
fn neighbours_are_appended_after_the_ranked_hits() {
    let ranked = vec!["a".to_string(), "b".to_string()];
    let neighbours = vec!["n1".to_string(), "n2".to_string()];
    let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
    assert_eq!(merged, vec!["a", "b", "n1", "n2"]);
}

/// A hit with many links must not flood the prompt with speculation.
#[test]
fn the_neighbour_cap_holds() {
    let ranked = vec!["a".to_string()];
    let neighbours: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
    let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
    assert_eq!(merged.len(), 1 + NEIGHBOUR_MAX);
}

/// An artifact already retrieved must not appear twice.
#[test]
fn a_neighbour_that_is_already_a_hit_is_dropped() {
    let ranked = vec!["a".to_string(), "b".to_string()];
    let neighbours = vec!["b".to_string(), "c".to_string()];
    let merged = append_neighbours(ranked, neighbours, NEIGHBOUR_MAX);
    assert_eq!(merged, vec!["a", "b", "c"]);
}

/// With no cliff there is no reliable part of the list to anchor on, so the
/// top three are used outright rather than none.
#[test]
fn anchors_fall_back_to_the_top_three_when_there_is_no_cliff() {
    assert_eq!(anchor_count(None, 10), 3);
    assert_eq!(anchor_count(Some(2), 10), 2, "never more anchors than the cliff allows");
    assert_eq!(anchor_count(Some(9), 10), 3, "never more than NEIGHBOUR_ANCHORS");
    assert_eq!(anchor_count(None, 2), 2, "never more anchors than there are hits");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib core::ask::retrieve store::artifacts`
Expected: FAIL — `append_neighbours`, `anchor_count`, `adjacent_artifacts` not found.

- [ ] **Step 3: Implement the store method**

In `src/store/artifacts.rs`:

```rust
    /// The artifacts either side of `ordinal` in the same corpus.
    ///
    /// The answer to a situation is often the paragraph after the one that
    /// matched. `ordinal` is already a continuous per-corpus sequence
    /// (`resequence` keeps it so), which is what makes this a lookup instead of
    /// a search. An edge returns the one side that exists.
    pub async fn adjacent_artifacts(&self, corpus_id: &str, ordinal: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts
             WHERE corpus_id = ? AND ordinal IN (?, ?) AND status = 'active'
             ORDER BY ordinal",
        )
        .bind(corpus_id)
        .bind(ordinal - 1)
        .bind(ordinal + 1)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(chunk_of_row).collect())
    }
```

(`chunk_of_row` is whatever the existing row-mapping helper in that file is
called — reuse it; do not write a second mapper.)

- [ ] **Step 4: Implement the merge helpers**

In `src/core/ask/retrieve.rs`:

```rust
/// How many of the top hits get their neighbours pulled in.
pub(super) const NEIGHBOUR_ANCHORS: usize = 3;

/// Total neighbours admitted, however many links the anchors have between them.
/// Speculation is useful in small quantities and is noise in large ones.
pub(super) const NEIGHBOUR_MAX: usize = 6;

/// Which hits to reach sideways from.
///
/// Above the cliff, capped at `NEIGHBOUR_ANCHORS`. With no cliff the top three
/// outright: "no cliff" means there is no basis for calling any part of the
/// list the reliable part, and reaching from nothing would disable the feature
/// on exactly the lists that need help most.
pub(super) fn anchor_count(cliff_at: Option<usize>, hits: usize) -> usize {
    cliff_at.unwrap_or(hits).min(NEIGHBOUR_ANCHORS).min(hits)
}

/// Ranked hits first, then neighbours, deduped, capped.
///
/// The ordering is the safety property, not a presentation choice. A neighbour
/// has no score comparable to a retrieved hit — it was reached, not retrieved —
/// so interleaving would corrupt the cliff that was just computed over those
/// scores. Appending also makes a neighbour the first thing the budget drops,
/// which is right: it is the most speculative excerpt in the prompt.
pub(super) fn append_neighbours(
    ranked: Vec<String>,
    neighbours: Vec<String>,
    cap: usize,
) -> Vec<String> {
    let mut out = ranked;
    let mut added = 0usize;
    for n in neighbours {
        if added == cap {
            break;
        }
        if !out.contains(&n) {
            out.push(n);
            added += 1;
        }
    }
    out
}
```

- [ ] **Step 5: Wire it into `ask`**

In `src/core/ask/mod.rs`, between the search and the packing: compute
`cliff_at = search::cliff(&scores)`; take `anchor_count(cliff_at, hits.len())`
leading hits; for each, collect `store.adjacent_artifacts(&h.corpus_id, h.ordinal)`
ids and `store.links_from(&[h.artifact_id.clone()], …)` ids; pass the id list
through `append_neighbours` against the ranked ids; then hydrate the added ids
into `SearchResult`s via `store.get_artifact` and push them onto `hits` before
building `blocks`.

Use the association config already on `Core` for the `links_from` arguments —
`self.associate.half_life_days`, `self.associate.show_min` — so the one-hop
reach obeys the same bounds the results rail does. `limit` is `NEIGHBOUR_MAX as i64`.

- [ ] **Step 6: Run tests**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/store/artifacts.rs src/core/ask
git commit -m "feat(ask): pull one hop sideways into the candidate pool

Adjacent ordinals and one-hop associations of the hits above the cliff,
deduped and capped at six. Appended after the ranked hits, never
interleaved: a reached artifact has no score comparable to a retrieved
one, and interleaving would corrupt the cliff just computed over them."
```

### Task 5: The literal check on the answer

**Files:**
- Create: `src/core/ask/check.rs`
- Modify: `src/core/ask/mod.rs` (`AskResponse` gains `unsupported`)
- Modify: `src/web/templates/_answer.html`
- Modify: `src/web/ui.rs` (`AnswerTemplate` gains `unsupported`)
- Modify: `assets/app.css`
- Modify: `src/eval/metrics.rs`, `tests/eval.rs`

**Interfaces:**
- Consumes: `crate::infer::verify::missing_literals(&str, &[String], &str) -> Vec<String>`.
- Produces: `pub(super) fn unsupported_literals(answer: &str, excerpts: &[String]) -> Vec<String>`; `pub fn mark_unsupported(html: &str, literals: &[String]) -> String`; `AskResponse::unsupported: Vec<String>`.

- [ ] **Step 1: Write the failing test**

Create `src/core/ask/check.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The fidelity thesis extended to generation: an answer cannot carry a
    /// literal the excerpts did not.
    #[test]
    fn a_command_in_no_excerpt_is_unsupported() {
        let excerpts = vec!["Run `systemctl restart engram` to apply it.".to_string()];
        let answer = "Run `systemctl restart engram`, then `rm -rf /var/lib/engram`.";
        let got = unsupported_literals(answer, &excerpts);
        assert!(
            got.iter().any(|l| l.contains("rm -rf /var/lib/engram")),
            "the invented command must be flagged: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.contains("systemctl restart engram")),
            "a command that is in an excerpt must not be: {got:?}"
        );
    }

    /// Nothing invented, nothing flagged — the common case, and the one that
    /// must not produce a badge on every answer.
    #[test]
    fn an_answer_drawn_entirely_from_its_excerpts_flags_nothing() {
        let excerpts = vec!["Set `fetch_max_bytes = 8388608` in config.toml.".to_string()];
        let answer = "Set `fetch_max_bytes = 8388608` in config.toml.";
        assert!(unsupported_literals(answer, &excerpts).is_empty());
    }

    /// Marking happens inside code fences too. A fabricated command is exactly
    /// the case this exists for, and exempting the place literals actually live
    /// would make the check decorative.
    #[test]
    fn marking_reaches_inside_a_code_block() {
        let html = "<pre><code>rm -rf /var/lib/engram</code></pre>";
        let marked = mark_unsupported(html, &["rm -rf /var/lib/engram".to_string()]);
        assert!(marked.contains(r#"<mark class="unsupported">"#), "{marked}");
    }

    /// The marker must never be able to inject markup: a literal is model
    /// output, and model output is untrusted.
    #[test]
    fn a_literal_containing_markup_cannot_break_out() {
        let html = "<p>&lt;script&gt;x&lt;/script&gt;</p>";
        let marked = mark_unsupported(html, &["<script>x</script>".to_string()]);
        assert!(!marked.contains("<script>"), "raw script tag leaked: {marked}");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib core::ask::check`
Expected: FAIL — `unsupported_literals` not found.

- [ ] **Step 3: Implement**

```rust
//! Checking an answer against what the model was actually shown.

/// Literals the answer carries that appear in none of its excerpts.
///
/// The same guard `jobs::window` already applies to every synthesised artifact
/// (`verify::missing_literals`), pointed at generation instead of synthesis. A
/// number, command or path that appears in no cited excerpt is not a fact the
/// base holds — it is the model's own, and the page must say so rather than let
/// it be read as retrieved.
///
/// No inference: this is a string operation over text already generated.
pub(super) fn unsupported_literals(answer: &str, excerpts: &[String]) -> Vec<String> {
    if excerpts.is_empty() {
        return Vec::new();
    }
    crate::infer::verify::missing_literals(answer, &[], &excerpts.join("\n\n"))
}

/// Wrap each unsupported literal in the rendered answer.
///
/// Operates on already-escaped HTML, and escapes the needle the same way before
/// searching, so a literal is matched as the text a reader sees. That is also
/// what makes it safe: the needle never re-enters the document as markup.
///
/// Longest first, so a literal that contains a shorter one is marked whole
/// rather than being broken in half by the shorter match landing first.
pub fn mark_unsupported(html: &str, literals: &[String]) -> String {
    let mut ordered: Vec<&String> = literals.iter().collect();
    ordered.sort_by_key(|l| std::cmp::Reverse(l.len()));
    let mut out = html.to_string();
    for lit in ordered {
        let needle = askama::filters::escape(askama::filters::Html, lit)
            .map(|e| e.to_string())
            .unwrap_or_else(|_| lit.clone());
        if needle.is_empty() {
            continue;
        }
        out = out.replace(
            &needle,
            &format!("<mark class=\"unsupported\">{needle}</mark>"),
        );
    }
    out
}
```

Add `unsupported: Vec<String>` to `AskResponse` with a doc comment saying why it
is there, populate it after the completion in `Core::ask`, and set it to an empty
vec in the two early-return abstention paths.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib core::ask`
Expected: PASS.

- [ ] **Step 5: Show it on the page**

In `src/web/templates/_answer.html`, inside `card-head` after the `truncated` badge:

```html
    {% if !unsupported.is_empty() %}
      <span class="card-meta"><span class="badge badge-warning"
        title="These appear in no cited excerpt. The model wrote them; the base does not hold them.">
        {{ unsupported.len() }} unsupported literal(s)</span></span>
    {% endif %}
```

and change `<div class="md">{{ answer|safe }}</div>` to render the marked HTML.
Add `unsupported: Vec<String>` to `AnswerTemplate` in `src/web/ui.rs` and pass
`check::mark_unsupported(&rendered, &resp.unsupported)` as `answer`.

In `assets/app.css`:

```css
mark.unsupported {
  background: var(--warn-bg, #4a3a00);
  color: inherit;
  border-bottom: 1px dashed var(--warn, #d9a400);
  padding: 0 .1em;
}
```

- [ ] **Step 6: Count it in the harness**

In `src/eval/metrics.rs`:

```rust
/// Answers carrying at least one literal none of their excerpts did.
///
/// The number phase 2 exists to move. Zero is the target; a rise after a
/// retrieval change means the change fed the model excerpts it then
/// over-reached from.
/// `counts` is one entry per judged answer: how many unsupported literals it
/// carried.
pub fn unsupported_rate(counts: &[usize]) -> f32 {
    if counts.is_empty() {
        return 0.0;
    }
    counts.iter().filter(|n| **n > 0).count() as f32 / counts.len() as f32
}
```

and report it from `evaluate_ask` in `tests/eval.rs` alongside citation recall.

- [ ] **Step 7: Run everything and commit**

Run: `cargo fmt && cargo clippy --all-targets && cargo test`

```bash
git add src/core/ask src/web src/eval assets
git commit -m "feat(ask): check the answer's literals against its excerpts

The same guard jobs::window applies to synthesis, pointed at generation.
Marked inside code fences too: a fabricated command is the case this
exists for. No inference; harness counts it."
```

---

# Phase 3 — Streaming

### Task 6: `Delta` and the defaulted trait method

The default implementation is what keeps this cheap: every implementor and every
existing test keeps working, and only `HttpCompleter` overrides it.

**Files:**
- Modify: `src/infer/mod.rs:89-119`
- Test: `src/infer/fake.rs` (inline)

**Interfaces:**
- Produces: `pub enum Delta { Token(String), Reasoning(String) }`; `Completer::answer_streaming(&self, &str, &str, usize, tokio::sync::mpsc::Sender<Delta>) -> Result<Completion>`.

- [ ] **Step 1: Write the failing test**

In `src/infer/fake.rs` tests:

```rust
/// The default implementation is the compatibility guarantee: an implementor
/// that knows nothing about streaming still streams, as one delta. Without it
/// every fake in the test suite would need a hand-written override.
#[tokio::test]
async fn a_completer_without_an_override_streams_its_whole_answer_as_one_delta() {
    let c = FakeCompleter::new("the answer");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let done = c.answer_streaming("sys", "usr", 128, tx).await.unwrap();
    let mut got = String::new();
    while let Some(d) = rx.recv().await {
        if let crate::infer::Delta::Token(t) = d {
            got.push_str(&t);
        }
    }
    assert_eq!(got, "the answer");
    assert_eq!(done.text, "the answer");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib infer::fake`
Expected: FAIL — no method `answer_streaming`.

- [ ] **Step 3: Implement**

In `src/infer/mod.rs`:

```rust
/// One piece of a streamed completion.
///
/// Reasoning is kept apart from the answer rather than concatenated: the page
/// shows it dimmed and above, and it is never part of what the literal check
/// or the citation parser reads.
#[derive(Debug, Clone)]
pub enum Delta {
    Token(String),
    Reasoning(String),
}
```

and on the trait, after `answer`:

```rust
    /// `answer`, delivering the text as it arrives.
    ///
    /// Defaults to `answer` followed by one delta, so an implementor that
    /// cannot stream — every fake in the test suite, and any endpoint without
    /// SSE — still satisfies the streaming caller without a hand-written
    /// override. Only `HttpCompleter` overrides this.
    ///
    /// The returned `Completion` is authoritative: a caller assembles its
    /// answer from it, not from the deltas it accumulated, so a dropped
    /// receiver can never silently truncate a stored answer.
    async fn answer_streaming(
        &self,
        system: &str,
        user: &str,
        ceiling: usize,
        sink: tokio::sync::mpsc::Sender<Delta>,
    ) -> Result<Completion> {
        let c = self.answer(system, user, ceiling).await?;
        let _ = sink.send(Delta::Token(c.text.clone())).await;
        Ok(c)
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: PASS, with no other file edited.

- [ ] **Step 5: Commit**

```bash
git add src/infer
git commit -m "feat(infer): Delta and a defaulted Completer::answer_streaming

Defaulted so every existing implementor and test keeps working; only
HttpCompleter will override it. The returned Completion stays
authoritative, so a dropped receiver cannot truncate a stored answer."
```

### Task 7: `HttpCompleter` streams over SSE

**Files:**
- Modify: `Cargo.toml` (reqwest `stream` feature)
- Modify: `src/infer/openai.rs` (`answer` at line 881; add the override below it)
- Test: `src/infer/openai.rs` (inline, with `wiremock` as the existing tests do)

**Interfaces:**
- Consumes: `Delta`, `Completion`.
- Produces: `HttpCompleter::answer_streaming` override; `pub(crate) fn parse_sse_line(line: &str) -> Option<SseChunk>`.

- [ ] **Step 1: Write the failing test**

```rust
/// Endpoints disagree on the field name for reasoning tokens: llama.cpp and
/// vLLM send `reasoning_content`, others send `reasoning`. Both must land in
/// the same place or the thinking silently vanishes on half of them.
#[test]
fn a_delta_carries_reasoning_under_either_field_name() {
    let a = parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#).unwrap();
    let b = parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning":"hm"}}]}"#).unwrap();
    assert_eq!(a.reasoning.as_deref(), Some("hm"));
    assert_eq!(b.reasoning.as_deref(), Some("hm"));
}

/// The sentinel ends the stream and is not a chunk.
#[test]
fn the_done_sentinel_is_not_a_chunk() {
    assert!(parse_sse_line("data: [DONE]").is_none());
    assert!(parse_sse_line("").is_none());
    assert!(parse_sse_line(": keep-alive").is_none());
}

/// Truncation is still detectable, now from the final chunk rather than the
/// whole response. Without it an answer cut off mid-sentence is
/// indistinguishable from a complete one.
#[test]
fn a_finish_reason_of_length_is_read_from_the_last_chunk() {
    let c = parse_sse_line(r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
    assert_eq!(c.finish_reason.as_deref(), Some("length"));
}

/// A JSON object can be split across two TCP reads. A parser that assumes one
/// chunk is one line loses tokens on exactly the long answers streaming exists
/// for, and does it intermittently.
#[tokio::test]
async fn a_data_line_split_across_two_reads_is_reassembled() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let c = completer_against(&server.uri());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let done = c.answer_streaming("s", "u", 64, tx).await.unwrap();
    let mut got = String::new();
    while let Some(Delta::Token(t)) = rx.recv().await {
        got.push_str(&t);
    }
    assert_eq!(got, "hello");
    assert_eq!(done.text, "hello");
}
```

(`completer_against` — reuse whatever the existing openai tests use to build an
`HttpCompleter` pointed at a mock server; read the tests around line 1528 and
follow them.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib infer::openai`
Expected: FAIL — `parse_sse_line` not found.

- [ ] **Step 3: Add the reqwest feature**

In `Cargo.toml`, add `"stream"` to reqwest's feature list (both the main and
dev-dependency entries, lines 33 and 69).

- [ ] **Step 4: Implement**

```rust
/// One `data:` frame of an OpenAI-shaped stream.
#[derive(Debug, Default)]
pub(crate) struct SseChunk {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
}

/// Parse one line. `None` for anything that is not a chunk: blank lines,
/// comment lines, and the `[DONE]` sentinel.
pub(crate) fn parse_sse_line(line: &str) -> Option<SseChunk> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let choice = v.get("choices")?.get(0)?;
    let delta = choice.get("delta");
    let s = |o: Option<&serde_json::Value>, k: &str| {
        o.and_then(|d| d.get(k))
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    Some(SseChunk {
        content: s(delta, "content"),
        // Endpoints disagree; accept both spellings or the thinking vanishes
        // on half of them.
        reasoning: s(delta, "reasoning_content").or_else(|| s(delta, "reasoning")),
        finish_reason: choice
            .get("finish_reason")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    })
}
```

Then the override on `impl Completer for HttpCompleter`: build the same body
`answer` builds, add `"stream": true`, send it, and read `bytes_stream()` into a
`String` buffer. Drain **complete lines only** — split on `\n`, keep the trailing
partial in the buffer — because a JSON object can be split across reads. Push
`Delta::Token` / `Delta::Reasoning` into the sink as they parse, accumulate
`content` into the answer, and remember the last non-null `finish_reason`.
Return `Completion { text, truncated: finish_reason.as_deref() == Some("length") }`.

Ignore send errors on the sink: a browser that closed the tab must not fail the
call that is still being recorded.

- [ ] **Step 5: Run tests**

Run: `cargo test --lib infer::openai`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/infer/openai.rs
git commit -m "feat(infer): stream completions from the endpoint over SSE

Line-buffered across reads, because a JSON object can split across two
of them. Reasoning read under both field names endpoints use for it."
```

### Task 8: `Core::ask` becomes a collector

**Files:**
- Modify: `Cargo.toml` (`async-stream`)
- Modify: `src/core/ask/mod.rs`
- Create: `src/core/ask/stream.rs`
- Test: `src/core/ask/mod.rs` (inline)

**Interfaces:**
- Consumes: `Delta`, `Completer::answer_streaming`.
- Produces: `pub enum AskEvent`; `Core::ask_events(&self, &AskRequest, impl Into<Origin>) -> impl futures_core::Stream<Item = Result<AskEvent>> + 'static`; `Core::ask` unchanged in signature.

- [ ] **Step 1: Write the failing test**

```rust
/// The two doors must never drift. `/api/v1/ask` and MCP collect; the page
/// streams; both must describe the same ask. This is the test that keeps them
/// honest as the code changes.
#[tokio::test]
async fn the_collected_answer_equals_the_streamed_one() {
    let core = test_core_with_artifacts().await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };

    let blocking = core.ask(&req, Door::Api).await.unwrap();

    let mut streamed = None;
    let s = core.ask_events(&req, Door::Api);
    tokio::pin!(s);
    while let Some(ev) = s.next().await {
        if let AskEvent::Done(d) = ev.unwrap() {
            streamed = Some(*d);
        }
    }
    let streamed = streamed.expect("the stream must terminate with Done");
    assert_eq!(blocking.answer, streamed.answer);
    assert_eq!(blocking.abstained, streamed.abstained);
    assert_eq!(blocking.dropped, streamed.dropped);
    assert_eq!(blocking.unsupported, streamed.unsupported);
    assert_eq!(
        blocking.citations.len(),
        streamed.citations.len(),
        "the same excerpts were shown to the model"
    );
}

/// The rail must be readable while the model is still writing, which means the
/// excerpts have to arrive before the first token.
#[tokio::test]
async fn citations_arrive_before_the_first_token_and_done_is_last() {
    let core = test_core_with_artifacts().await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };
    let mut order: Vec<&'static str> = vec![];
    let s = core.ask_events(&req, Door::Api);
    tokio::pin!(s);
    while let Some(ev) = s.next().await {
        order.push(match ev.unwrap() {
            AskEvent::Retrieved { .. } => "retrieved",
            AskEvent::Needs(_) => "needs",
            AskEvent::Citations(_) => "citations",
            AskEvent::Reasoning(_) => "reasoning",
            AskEvent::Token(_) => "token",
            AskEvent::Done(_) => "done",
        });
    }
    let first_token = order.iter().position(|e| *e == "token");
    let citations = order.iter().position(|e| *e == "citations").expect("citations emitted");
    if let Some(t) = first_token {
        assert!(citations < t, "citations must precede the first token: {order:?}");
    }
    assert_eq!(order.last(), Some(&"done"), "Done must be terminal: {order:?}");
}

/// An ask is recorded once, whichever door it came through — the harness reads
/// these rows and would double-count otherwise.
#[tokio::test]
async fn a_streamed_ask_is_recorded_exactly_once() {
    let core = test_core_with_artifacts().await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };
    let before = core.store.ask_stats().await.unwrap().total;
    let s = core.ask_events(&req, Door::Ui);
    tokio::pin!(s);
    while s.next().await.is_some() {}
    let after = core.store.ask_stats().await.unwrap().total;
    assert_eq!(after, before + 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib core::ask`
Expected: FAIL — `ask_events` not found.

- [ ] **Step 3: Add the dependency**

In `Cargo.toml`: `async-stream = "0.3"`.

- [ ] **Step 4: Implement**

`src/core/ask/stream.rs`:

```rust
//! What one ask emits while it happens.

use crate::core::search::SearchResult;

/// One step of an ask, in the order it occurs.
///
/// The page renders these; `Core::ask` collects them back into an
/// `AskResponse`. Having exactly one producer is what keeps the streaming and
/// blocking doors describing the same ask.
#[derive(Debug, Clone)]
pub enum AskEvent {
    /// A retrieval round finished. `round` is 1, or 2 for the follow-up.
    Retrieved {
        round: u8,
        shown: usize,
        dropped: usize,
        cliff_at: Option<usize>,
    },
    /// What the model said it still needed. Round 2 only.
    Needs(String),
    /// The excerpts the model will see. Emitted once, after the final
    /// retrieval and before the first token, so the rail is readable while the
    /// answer is still being written.
    Citations(Vec<SearchResult>),
    Reasoning(String),
    Token(String),
    /// Terminal, and carries exactly what the blocking door returns.
    Done(Box<super::AskResponse>),
}
```

In `mod.rs`, restructure: move the whole body of the current `ask` into
`ask_events`, built with `async_stream::try_stream!`. It owns `let core = self.clone();`
so the stream is `'static` — an SSE response outlives the handler that made it.
The interactive lane is taken inside the stream body and held to the end.

At the completion, spawn the sink: create an `mpsc::channel::<Delta>(64)`, call
`core.completer.answer_streaming(...)` in a `tokio::spawn`, and `yield` a
`Token`/`Reasoning` event for each delta received. Await the join handle for the
authoritative `Completion`, run the literal check, `record_ask`, then
`yield AskEvent::Done(...)`.

Then:

```rust
    /// Ask, and wait for the whole answer.
    ///
    /// A collector over `ask_events`, not a second implementation: `/api/v1/ask`
    /// and the MCP tool cannot stream and are not asked to, and there must be
    /// exactly one account of what asking means.
    pub async fn ask(&self, req: &AskRequest, origin: impl Into<Origin>) -> Result<AskResponse> {
        let s = self.ask_events(req, origin);
        tokio::pin!(s);
        let mut done = None;
        while let Some(ev) = s.next().await {
            if let AskEvent::Done(d) = ev? {
                done = Some(*d);
            }
        }
        done.ok_or_else(|| Error::Internal("ask produced no answer".into()))
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test`
Expected: PASS, including every pre-existing ask, api and mcp test unchanged.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/core/ask
git commit -m "feat(ask): one producer, two doors

Core::ask_events is the implementation; Core::ask collects it. The API
and MCP doors keep their signatures and there is one account of what
asking means. An equivalence test holds the two together."
```

### Task 9: The SSE route

**Files:**
- Modify: `src/web/state.rs`
- Modify: `src/web/ui.rs` (`ask_submit` at line 1935; new route at line 2298)
- Test: `src/web/ui.rs` (inline)

**Interfaces:**
- Consumes: `Core::ask_events`.
- Produces: `AppState::ask_handoff: Arc<Mutex<HashMap<String, (AskRequest, Instant)>>>` with `AppState::ask_handoff_park(&self, req: AskRequest) -> String` and `AppState::ask_handoff_take(&self, id: &str) -> Option<AskRequest>`; `pub const ASK_HANDOFF_TTL: Duration`; `POST /ui/ask` → `{"id": "..."}`; `GET /ui/ask/{id}/stream`.

- [ ] **Step 1: Write the failing test**

```rust
/// EventSource is GET-only, and a GET that runs a model call and writes a row
/// is the kind history and prefetchers replay. The id is the guard, and it is
/// one-shot.
#[tokio::test]
async fn an_ask_handoff_id_cannot_be_used_twice() {
    let (app, cookie, _core) = app_session_and_core_with_feedback().await;
    let id = post_ask(&app, &cookie, "chunk").await;
    let first = get_stream(&app, &cookie, &id).await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = get_stream(&app, &cookie, &id).await;
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
}

/// An unknown id is a 404, never a fresh ask against an empty question.
#[tokio::test]
async fn an_unknown_handoff_id_is_not_found() {
    let (app, cookie, _core) = app_session_and_core_with_feedback().await;
    let res = get_stream(&app, &cookie, "nope").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// The stream is SSE and terminates with the done event carrying the rendered
/// answer, which is what the page swaps in.
#[tokio::test]
async fn the_stream_ends_with_a_done_event_carrying_rendered_html() {
    let (app, cookie, _core) = app_session_and_core_with_feedback().await;
    let id = post_ask(&app, &cookie, "chunk").await;
    let res = get_stream(&app, &cookie, &id).await;
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let body = body_of(res).await;
    assert!(body.contains("event: done"), "{body}");
    assert!(body.contains("<div class=\"md\">"), "the done event carries HTML: {body}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib web::ui`
Expected: FAIL — no such route.

- [ ] **Step 3: Implement the handoff**

In `src/web/state.rs`, on `AppState`:

```rust
    /// Questions parked between the POST that creates them and the GET that
    /// streams them.
    ///
    /// `EventSource` is GET-only, and a GET that runs a model call and writes
    /// an `ask_events` row is a mutating GET — the kind browser history and
    /// prefetchers replay. An opaque one-shot id costs no schema and cannot be
    /// replayed. Entries are removed on consumption and swept on insert.
    pub ask_handoff: Arc<Mutex<HashMap<String, (crate::core::ask::AskRequest, Instant)>>>,
```

with `pub const ASK_HANDOFF_TTL: Duration = Duration::from_secs(60);` beside it,
and this pair, which sweeps expired entries on every park so the map cannot
grow without bound on a page nobody streamed:

```rust
impl AppState {
    pub fn ask_handoff_park(&self, req: crate::core::ask::AskRequest) -> String {
        let id = crate::store::new_id();
        if let Ok(mut m) = self.ask_handoff.lock() {
            let now = Instant::now();
            m.retain(|_, (_, at)| now.duration_since(*at) < ASK_HANDOFF_TTL);
            m.insert(id.clone(), (req, now));
        }
        id
    }

    /// One shot: the entry is removed whether or not the stream succeeds.
    pub fn ask_handoff_take(&self, id: &str) -> Option<crate::core::ask::AskRequest> {
        let mut m = self.ask_handoff.lock().ok()?;
        let (req, at) = m.remove(id)?;
        (Instant::now().duration_since(at) < ASK_HANDOFF_TTL).then_some(req)
    }
}
```

- [ ] **Step 4: Implement the routes**

`ask_submit` becomes: validate, `park`, return `Json(json!({"id": id}))`.

The stream handler:

```rust
async fn ask_stream(
    State(st): State<AppState>,
    _id: Identity,
    Path(handoff): Path<String>,
) -> Result<Response> {
    let req = st.ask_handoff_take(&handoff).ok_or(Error::NotFound)?;
    let core = st.core.clone();
    let events = async_stream::stream! {
        let s = core.ask_events(&req, Door::Ui);
        tokio::pin!(s);
        while let Some(ev) = s.next().await {
            yield match ev {
                Ok(e) => sse_event(&core, e).await,
                Err(e) => Ok(SseEvent::default().event("error").data(e.to_string())),
            };
        }
    };
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}
```

`sse_event` maps each `AskEvent` to a named SSE event with a JSON payload, and
for `Done` renders the final fragment server-side: `markdown::render`, then
`check::mark_unsupported`, then citation linkification. Nothing renders markdown
in JavaScript.

Register: `.route("/ui/ask/{id}/stream", get(ask_stream))`.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test --lib web`

```bash
git add src/web
git commit -m "feat(web): stream ask over SSE behind a one-shot id

POST parks the question and returns an id; GET consumes it and streams.
Keeps the model call off a replayable GET without a schema change."
```

### Task 10: The page

**Files:**
- Modify: `src/web/templates/ask.html`, `_answer.html`
- Modify: `assets/app.js`, `assets/app.css`
- Test: manual, per steps below

**Interfaces:**
- Consumes: `POST /ui/ask` → `{id}`, `GET /ui/ask/{id}/stream`.
- Produces: nothing other code reads.

- [ ] **Step 1: Rewrite `ask.html`**

Drop `hx-post`; the form gets `id="ask-form"`. Add, above `#ask-result`:

```html
<div id="ask-reasoning" class="reasoning" hidden></div>
<pre id="ask-live" class="answer-live" hidden></pre>
<div id="ask-rail" class="rail" role="list" aria-label="Excerpts"></div>
<div id="ask-result"></div>
```

- [ ] **Step 2: Add the driver to `app.js`**

Submit handler: `POST /ui/ask` as form data, read `{id}`, open
`new EventSource('/ui/ask/' + id + '/stream')`, then:

- `citations` → render the rail items (server sends the HTML fragment).
- `reasoning` → append to `#ask-reasoning`, unhide it.
- `token` → append to `#ask-live`, unhide it.
- `done` → put the payload's HTML into `#ask-result`, hide `#ask-live` and
  `#ask-reasoning`, close the source.
- `error` → show the message in `#ask-result`, close the source.

Always `close()` the EventSource on `done` and `error`; without it the browser
reconnects and asks the question again, which costs a second model call.

- [ ] **Step 3: Citation links**

In the `done` HTML the server has already turned each `[n]` into
`<a class="cite" href="#cite-n">[n]</a>`. In `app.js`, delegate clicks on
`.cite`: `scrollIntoView` the matching `#cite-n` rail item and toggle a
`.rail-active` class on it.

- [ ] **Step 4: Style it**

In `app.css`, add `.answer-live` (monospace, pre-wrap, muted), `.reasoning`
(dimmed, smaller, collapsible), `.cite` (superscript-ish link), `.rail-active`
(the same emphasis `_results.html` uses for a selected row — reuse the existing
custom property rather than a new colour).

- [ ] **Step 5: Verify in the running app**

Run the app and ask a question with a configured endpoint. Confirm: tokens
appear progressively; the rail is populated before the first token; `[n]`
scrolls to its excerpt; the final render replaces the live text exactly once;
an unsupported literal is marked.

- [ ] **Step 6: Commit**

```bash
git add src/web/templates assets
git commit -m "feat(ui): stream the answer, link its citations to a rail

The page needs JS from here on; the API and MCP doors are the JS-free
way in. Rendering stays server-side: the done event carries the HTML."
```

---

# Phase 4 — The second round, and keeping an answer

### Task 11: The follow-up completer

**Files:**
- Modify: `src/infer/prompt.rs`
- Modify: `src/infer/openai.rs`
- Modify: `src/core/mod.rs`
- Test: `src/infer/prompt.rs` (inline)

**Interfaces:**
- Produces: `FOLLOW_UP_SYSTEM`, `follow_up_prompt(question: &str, excerpts: &[String]) -> String`, `follow_up_schema() -> serde_json::Value`, `parse_follow_up(&str) -> Option<String>`; `HttpCompleter::for_follow_up(&TierConfig) -> Self`; `Core::follow_up: Option<Arc<dyn Completer>>`.

- [ ] **Step 1: Write the failing test**

```rust
/// `null` is the common answer and must be readable as "I have enough",
/// never as a query to run.
#[test]
fn a_null_need_parses_as_nothing_further() {
    assert_eq!(parse_follow_up(r#"{"need": null}"#), None);
    assert_eq!(parse_follow_up(r#"{"need": ""}"#), None);
    assert_eq!(parse_follow_up(r#"{"need": "   "}"#), None);
}

#[test]
fn a_need_parses_as_the_query_to_run() {
    assert_eq!(
        parse_follow_up(r#"{"need": "engram retention ticker interval"}"#),
        Some("engram retention ticker interval".to_string())
    );
}

/// The schemas are sent to the endpoint to constrain decoding, so a schema that
/// has drifted from its parser constrains the model into output the parser then
/// rejects — a failure that looks exactly like a bad model.
#[test]
fn a_reply_that_satisfies_the_follow_up_schema_parses() {
    let schema = follow_up_schema();
    assert!(schema["properties"]["need"].is_object());
    assert!(parse_follow_up(r#"{"need":"x"}"#).is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib infer::prompt`
Expected: FAIL — `parse_follow_up` not found.

- [ ] **Step 3: Implement**

```rust
/// The one question the retrieval loop is allowed to ask itself.
///
/// Deliberately narrow: it may name one more thing to look for, or nothing at
/// all. It is never asked to answer, to plan, or to say how many rounds it
/// wants — "let the model say once what it still needs" is the bounded version
/// of a mechanism whose unbounded version is an agent.
pub const FOLLOW_UP_SYSTEM: &str = r#"You are helping a search system decide whether it has enough material.

You are given a question and the excerpts retrieved for it. Decide whether the
excerpts together contain what is needed to answer.

Reply with JSON only, in exactly this shape:

{"need": "a short search query" }   or   {"need": null}

- null: the excerpts are sufficient. This is the common answer.
- a query: name the ONE thing that is missing, as the words you would search
  for. Not a question, not a sentence — a query. Never repeat the original
  question back."#;
```

`parse_follow_up` reuses `extract_json`, reads `need`, trims, and maps empty to
`None`. `for_follow_up` mirrors `for_judging` but takes a `&TierConfig` and
carries `("need", prompt::follow_up_schema())` as its response shape.

In `src/core/mod.rs` add the field and build it where the other judges are built:
`Some` only when `cfg.infer.ask.follow_up`, using
`cfg.infer.ask.follow_up_endpoint` (resolved in Task 1) and falling back to the
ask role's own endpoint when that is unset.

- [ ] **Step 4: Run tests and commit**

Run: `cargo test --lib infer`

```bash
git add src/infer src/core/mod.rs
git commit -m "feat(ask): the follow-up completer and its one question"
```

### Task 12: The second round

**Files:**
- Create: `src/core/ask/follow_up.rs`
- Modify: `src/core/ask/mod.rs`
- Test: `src/core/ask/mod.rs` (inline)

**Interfaces:**
- Consumes: `Core::follow_up`, `parse_follow_up`.
- Produces: `pub(super) async fn needed_query(core: &Core, question: &str, excerpts: &[String]) -> Option<String>`.

- [ ] **Step 1: Write the failing test**

```rust
/// Off by default, and "off" must mean no call at all — asserted on a counting
/// fake rather than inferred from the answer looking the same.
#[tokio::test]
async fn follow_up_off_makes_no_extra_call() {
    let core = test_core_with_counting_completers(false).await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };
    core.ask(&req, Door::Api).await.unwrap();
    assert_eq!(calls_to_follow_up(&core), 0);
}

/// On, and the model says it has enough: still exactly one retrieval.
#[tokio::test]
async fn a_null_need_skips_the_second_retrieval() {
    let core = test_core_with_follow_up_replying(r#"{"need": null}"#).await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };
    let mut rounds = 0;
    let s = core.ask_events(&req, Door::Api);
    tokio::pin!(s);
    while let Some(ev) = s.next().await {
        if let AskEvent::Retrieved { .. } = ev.unwrap() {
            rounds += 1;
        }
    }
    assert_eq!(rounds, 1);
}

/// On, and the model names something: exactly two retrievals and never three.
/// Bounded means bounded.
#[tokio::test]
async fn a_named_need_retrieves_exactly_once_more() {
    let core = test_core_with_follow_up_replying(r#"{"need": "ticker interval"}"#).await;
    let req = AskRequest { q: "chunk".into(), limit: None, tags: vec![], category: None };
    let mut rounds = 0;
    let mut needs = 0;
    let s = core.ask_events(&req, Door::Api);
    tokio::pin!(s);
    while let Some(ev) = s.next().await {
        match ev.unwrap() {
            AskEvent::Retrieved { .. } => rounds += 1,
            AskEvent::Needs(q) => {
                needs += 1;
                assert_eq!(q, "ticker interval");
            }
            _ => {}
        }
    }
    assert_eq!(rounds, 2, "exactly one extra round, never a loop");
    assert_eq!(needs, 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib core::ask`
Expected: FAIL — `needed_query` not found.

- [ ] **Step 3: Implement**

`needed_query` returns `None` immediately when `core.follow_up` is `None`, so the
disabled path costs nothing. Otherwise one `answer` call through the follow-up
completer, parsed with `parse_follow_up`. Any error returns `None` — a failed
follow-up degrades to the single-round answer rather than failing the ask.

In `ask_events`, after round 1's `Retrieved`: call `needed_query`; on `Some(q)`
yield `Needs(q)`, run the same retrieval path (search → neighbours → pack) for
`q`, merge deduped by `artifact_id` into the candidate list, re-pack, and yield a
second `Retrieved { round: 2, .. }`. There is no third.

- [ ] **Step 4: Run tests and commit**

Run: `cargo test --lib core::ask`

```bash
git add src/core/ask
git commit -m "feat(ask): one bounded extra retrieval round, off by default

Exactly one. Not a loop: 'say once what it still needs' is the bounded
version of a mechanism whose unbounded version is an agent. The harness
decides whether it earns its place."
```

### Task 13: Capture this answer

**Files:**
- Modify: `src/web/ui.rs` (capture page prefill; new route)
- Modify: `src/web/templates/capture.html`, `_answer.html`
- Test: `src/web/ui.rs` (inline)

**Interfaces:**
- Produces: `POST /ui/ask/{id}/capture` → redirect to `/ui/capture` with prefill; `CaptureTemplate::prefill_text: String`, `CaptureTemplate::prefill_note: String`.

- [ ] **Step 1: Write the failing test**

```rust
/// Prefilled, never saved. The save is the operator's decision, and that is the
/// line: this is a person keeping something the model wrote, not the system
/// writing memory to itself.
#[tokio::test]
async fn capturing_an_answer_prefills_the_page_without_storing_anything() {
    let (app, cookie, core) = app_session_and_core_with_feedback().await;
    let before = core.store.list_corpora(100, 0).await.unwrap().len();
    let ask_id = record_a_ui_ask(&core, "chunk").await;

    let res = post(&app, &cookie, &format!("/ui/ask/{ask_id}/capture"), "").await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);

    let after = core.store.list_corpora(100, 0).await.unwrap().len();
    assert_eq!(after, before, "capture must not save on its own");

    let page = get_body(&app, &cookie, res.headers()["location"].to_str().unwrap()).await;
    assert!(page.contains("fake answer"), "the answer is prefilled: {page}");
    assert!(page.contains("chunk"), "the question travels as provenance: {page}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib web::ui`
Expected: FAIL — no such route.

- [ ] **Step 3: Implement**

The route reads the `ask_events` row and its citations, builds the note
(`Answered from: <id>, <id>` plus the question), and redirects to
`/ui/capture?text=…&note=…`. `capture_page` gains the two query parameters and
passes them to the template as the textarea's value and a hidden provenance
field. The corpus is created with kind `ask` when the operator submits.

Add the button to `_answer.html`, beside the verdict bar, only when `event_id`
is set.

- [ ] **Step 4: Run tests and commit**

Run: `cargo test --lib web`

```bash
git add src/web
git commit -m "feat(ui): keep an answer, as a paste the operator approves

Prefills the capture page; does not save. The trace records that the
text was model-written and from what, and synthesis then treats it like
any other paste."
```

### Task 14: Documentation

**Files:**
- Modify: `ROADMAP.md`, `README.md`

- [ ] **Step 1: Update `ROADMAP.md`**

In `[Ask]`: move items 2 and 3 into the "What is built" paragraph at the top of
the file, and record item 1 as cut with its reason (generated text influencing
ranking is the wrong side of the fidelity line, even when never displayed).
Replace the "Model tiers" paragraph with a one-line statement that tiers are
built. In `[Retrieval]`, note that **Continues in** now has its store method and
its adjacency rule from the ask work, and that only the presentation remains.

- [ ] **Step 2: Update `README.md`**

Document `[infer.tiers.*]`, the `tier` key, `follow_up`, and that `/ui/ask`
requires JavaScript while `/api/v1/ask` and MCP do not.

- [ ] **Step 3: Commit**

```bash
git add ROADMAP.md README.md
git commit -m "docs: streaming ask, the retrieval loop and model tiers"
```

---

## Self-review

**Spec coverage.** §1 tiers → Task 1. §2 module split → Task 2; cliff → Task 3;
neighbours → Task 4; literal check → Task 5. §3 producer/collector → Task 8;
`answer_streaming` → Tasks 6–7; SSE route → Task 9; page, citations, rail →
Task 10. §4 second round → Tasks 11–12; capture → Task 13. Testing section →
distributed across each task's steps, plus the harness metric in Task 5.

**Deliberate ordering note.** `AskRole::follow_up` and `follow_up_endpoint` are
declared in Task 1 rather than Task 11, so the config shape is settled once and
Task 11 adds no second migration.

**Known risk carried from the spec.** `verify::extract_literals` was tuned on
synthesised artifacts, not prose. If Task 5's check over-fires on real answers,
the fallback is marking only within code spans and fences — **not** loosening
the extractor, which synthesis shares.
