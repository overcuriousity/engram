# KISS Shrink Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Shrink the engram repo from ~70k to ~40k lines by deleting finished-work docs, dead code, duplicated tests, internal duplication, and four superseded hygiene mechanisms — without removing any wired feature or in-place upgrade path.

**Architecture:** Five tiers, each a self-contained commit on branch `chore/kiss` that compiles and passes `cargo test`. Tier 0 docs → Tier 1 dead code + bug fixes → Tier 2 tests → Tier 3 internal dedupe → Tier 4 approved machinery. Later tiers depend on earlier ones only where stated.

**Tech Stack:** Rust 2024 / cargo, sqlx (SQLite), Qdrant REST, axum, askama, wiremock. Integration tests need Qdrant on :6333 (`docker compose up -d`).

**Spec:** `docs/superpowers/specs/2026-08-15-kiss-shrink-design.md`

## Global Constraints

- Every wired feature stays: all routes (incl. `PATCH /api/v1/artifacts/{id}`, `/api/v1/resurface`, `/api/v1/consolidation`, `/api/v1/consolidation/stale`, URL-fetch door), `Fold` coalescing, ask, all auth modes, MCP tools, extension.
- Every in-place upgrade path stays: Qdrant alias generations, `--reindex`, `--replace-legacy`, `--backfill-lifecycle`, `--recompute-coverage`, `ADDED_COLUMNS` migrator, dual `superseded`/`status` payload flag, lifecycle backfill, retired-config-key carry.
- **Never `DROP COLUMN` or remove a column from `schema.sql`.** Unused columns stay; stop reading them at most.
- Removed config keys (`consolidate.autonomous`, `consolidate.sample`, `pacing.breaker_after`, `pacing.breaker_probe_secs`, `tokenizer_path`) must be added to the retired/moved-key warning in `src/config.rs` so an old config file warns.
- No "removed X" / "used to be" comments. Deleted code lives in git.
- Line numbers below are anchors from the 2026-08-15 audit; **always re-grep before editing** — earlier tasks shift them.
- Gate for every task: `cargo build && cargo test` green (integration tests: `cargo test --test integration_qdrant` where the task says so, with Qdrant running). `cargo clippy` must not add new warnings.
- One commit per task, `chore(kiss): …` prefix, trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Report the line delta after each tier: `git diff --stat master..HEAD | tail -1`.

---

## Setup

### Task 0: Branch and baseline

- [x] **Step 1: Branch**
```bash
git checkout -b chore/kiss master
```
- [x] **Step 2: Baseline numbers** (record in the final report)
```bash
find . -name '*.rs' -not -path './target/*' | xargs wc -l | tail -1
find docs spec.md -name '*.md' | xargs wc -l | tail -1
cargo test 2>&1 | grep -E '^test result' 
```
- [x] **Step 3: Confirm Qdrant is available for integration tests**
```bash
docker compose up -d && curl -s localhost:6333/collections | head -c 200
```
If Qdrant cannot run, note it; Tiers 2 and 4 must then skip `integration_qdrant` and say so in the report.

---

## Tier 0 — docs

### Task 1: Delete finished-work docs

**Files:**
- Delete: `docs/superpowers/plans/*.md` except `2026-08-15-kiss-shrink.md` (this plan)
- Delete: `docs/superpowers/specs/*.md` except `2026-08-14-autonomous-consolidation-design.md` and `2026-08-15-kiss-shrink-design.md`
- Delete: `spec.md`

- [x] **Step 1: Verify nothing outside docs references them**
```bash
grep -rn 'docs/superpowers\|spec\.md' --include='*.rs' --include='*.md' --include='*.toml' --include='*.html' --include='*.js' . | grep -v '^./docs/' | grep -v '^./target/'
```
Expected: only ROADMAP.md lines pointing at `2026-08-14-autonomous-consolidation-design.md`.
- [x] **Step 2: Delete**
```bash
cd docs/superpowers/plans && ls | grep -v '2026-08-15-kiss-shrink.md' | xargs git rm -q && cd ../specs && ls | grep -v -e '2026-08-14-autonomous-consolidation-design.md' -e '2026-08-15-kiss-shrink-design.md' | xargs git rm -q && cd ../../.. && git rm -q spec.md
```
- [x] **Step 3: Check README/ROADMAP don't link `spec.md`**
```bash
grep -n 'spec\.md' README.md ROADMAP.md
```
Remove any hit's link text (keep the sentence if it stands alone).
- [x] **Step 4: Commit** — `chore(kiss): drop plans and specs for merged work, and the stale spec.md`

---

## Tier 1 — dead code + bug fixes

### Task 2: Delete uncalled functions

**Files:**
- Modify: `src/infer/fake.rs` (`FakeSynthesizer::failing_on` ~L111-123)
- Modify: `src/infer/verify.rs` (`span_is_plausible` ~L245-266, tests ~L462-475)
- Modify: `src/infer/openai.rs` (`probe` ~L601-624), `src/main.rs` (~L144-165, four `.probe(...).await;` calls)
- Modify: `src/error.rs` (`Duplicate` variant ~L13-14, `status()` arm ~L62)
- Modify: `src/infer/split.rs` (`is_heading_for_test` ~L22-25), `src/core/extract.rs` (its one assertion ~L203)

- [x] **Step 1: Prove each is uncalled**
```bash
for s in failing_on span_is_plausible 'fn probe\|\.probe(' 'Error::Duplicate' is_heading_for_test; do echo "== $s"; grep -rn "$s" src tests; done
```
Expected: definitions + (for probe) the four discarded call sites in `main.rs`; for `Error::Duplicate` only the `status()` arm; nothing else.
- [x] **Step 2: Delete them.** In `main.rs`, delete the whole "probe each role" block; if it was the only user of a `let` binding, delete that too. In `extract.rs` remove just the assertion line(s) using `is_heading_for_test`; keep the rest of the test.
- [x] **Step 3: `cargo build && cargo test`** — green.
- [x] **Step 4: Commit** — `chore(kiss): delete functions with no callers`

### Task 3: Stop writing EXIF `tags` catch-all; drop `StoreDrift` counters

**Files:**
- Modify: `src/core/image.rs` (~L171 doc line, ~L212-236 the "every other tag" loop with `MakerNote`/`UserComment`/`Tag(` filters and 200-char truncation; test assertion ~L517)
- Modify: `src/core/ingest.rs` (`StoreDrift` struct ~L38-54; `rows_restored`/`corpora_restored`/`points_requeued` increments inside `heal_store_drift` ~L760-905; the summary log ~L905-912 stays), `src/main.rs:~122`, `src/jobs/consolidate.rs:~387` (callers discard the value)

- [x] **Step 1: Verify readers**
```bash
grep -rn '"tags"\|\["tags"\]\|exif.*tags' src | grep -v test
grep -rn 'StoreDrift\|rows_restored\|corpora_restored\|points_requeued' src tests
```
Expected for exif tags: only the writer in `image.rs`. Readers of `metadata["exif"]` are `src/infer/prompt.rs` and `src/web/ui.rs` and read only `taken_at`, `camera`, `gps` — confirm.
- [x] **Step 2: Delete the `tags` loop and its doc line; keep `taken_at`, `camera`, `gps`, `orientation`.** Update the test at ~L517 to no longer assert on `tags`.
- [x] **Step 3: `heal_store_drift` returns `Result<()>`.** Keep three local `u64` counters only if the closing `tracing::info!` still reports them; delete `StoreDrift` and its doc. Update the ingest tests that read `.rows_restored` etc. to assert on the store instead (e.g. `core.store.get_artifact(id).await?.is_some()`), or delete an assertion that duplicated one the same test already makes on the store. (Note: Task 20 later replaces `heal_store_drift` with a warn; keep this change minimal — only enough to compile.)
- [x] **Step 4: `cargo test`** — green.
- [x] **Step 5: Commit** — `chore(kiss): stop storing exif tags nothing reads; drift repair returns unit`

### Task 4: Remove `eval-prepare` binary

**Files:**
- Delete: `src/bin/eval_prepare.rs`
- Modify: `Cargo.toml` (`[[bin]] name = "eval-prepare"` block + comment, ~L14-21)
- Modify: `tests/eval.rs` (~L62, 74, 80 error strings mentioning `eval-prepare`), README/ROADMAP if they mention it

- [x] **Step 1: Verify**
```bash
grep -rn 'eval-prepare\|eval_prepare' --exclude-dir=target .
```
- [x] **Step 2: Delete the file, the Cargo block, and reword the three `tests/eval.rs` strings** to point at `engram --export-eval <dir>` (README §"Learning what the search got wrong" documents it).
- [x] **Step 3: If `Store::memory()` in `src/store/mod.rs` has a doc comment justifying itself by `eval-prepare`, shorten it to "in-memory SQLite for tests and `--recompute-coverage`."** Do not delete the fn.
- [x] **Step 4: `cargo build --all-targets && cargo test`** — green.
- [x] **Step 5: Commit** — `chore(kiss): drop eval-prepare; --export-eval replaced it`

### Task 5: Bug fix — reconcile must not be gated by `consolidate.enabled`

**Files:**
- Modify: `src/jobs/consolidate.rs` (`run`: early `return` at ~L310 when `!enabled`; `reconcile::run(core)` at ~L316)
- Test: `src/jobs/consolidate.rs` tests

- [x] **Step 1: Write the failing test** (in `consolidate.rs` `mod tests`, using the existing `test_core`-style helper in that module; adapt names to what exists):
```rust
#[tokio::test]
async fn reconcile_runs_even_when_consolidation_is_disabled() {
    let core = test_core_with_consolidate_disabled().await; // build via existing helper, set config.consolidate.enabled = false
    // Seed a corpus stuck in a state reconcile repairs: e.g. status Synthesizing with all segments Done and no live jobs.
    // (Copy the seeding used by the existing reconcile test that asserts "a settled corpus is finished".)
    run(&core).await.unwrap();
    // Assert the corpus is now settled / has an Embed job — same assertion the reconcile test makes.
}
```
- [x] **Step 2: Run it** — `cargo test -p engram reconcile_runs_even_when_consolidation_is_disabled` — FAILS (nothing repaired).
- [x] **Step 3: Move `reconcile::run(core).await?;` above the `if !core.consolidate.enabled { return … }` check.** Keep the return shape unchanged.
- [x] **Step 4: Run tests** — green.
- [x] **Step 5: Commit** — `fix(consolidate): the repair sweep runs even with duplicate hygiene off`

### Task 6: Bug fix — remove hard-coded `theme: "light"`

**Files:**
- Modify: `src/web/ui.rs` (field on ~9 template structs: ~L298,327,366,411,420,498; assignments ~L516,584,825,1193,1430,1593), `src/web/judge.rs:~60,216`, `src/web/pair.rs:~93,138`, `src/web/extension.rs:~23,41`, `src/web/auth_routes.rs:~33,61,69,97`
- Modify: `src/web/templates/layout.html:2` (`data-theme="{{ theme }}"` attribute) and any other template referencing `theme`

- [x] **Step 1: Find every use**
```bash
grep -rn '\btheme\b' src/web src/web/templates assets/app.css | grep -v 'theme_color\|prefers-color-scheme'
```
- [x] **Step 2: Delete the field from every struct and every `theme: "light".into()` assignment; delete the `data-theme` attribute from `layout.html`.** Confirm `assets/app.css` has a `@media (prefers-color-scheme: dark)` block or that its `[data-theme="dark"]` block is now applied via a `prefers-color-scheme` media query. If it only has `[data-theme="dark"]`, wrap the same rules in `@media (prefers-color-scheme: dark) { :root { … } }` — copy the variables verbatim; do not redesign.
- [x] **Step 3: `cargo build && cargo test`** (askama compiles templates; a leftover `{{ theme }}` fails the build).
- [x] **Step 4: Commit** — `fix(ui): stop pinning every page to the light theme`

### Task 7: Tier 1 report
- [x] Tier 1 done: 41 files, −21,052 / +103; clippy 0 warnings; 893 unit tests green. (StoreDrift half of Task 3 deferred into Task 22.)

---

## Tier 2 — tests only (no impl change)

### Task 8: `vector/memory.rs` — delete tests twinned in `integration_qdrant.rs`

**Files:**
- Modify: `src/vector/memory.rs` `mod tests` (~L494-1099)
- Reference: `tests/integration_qdrant.rs`

- [ ] **Step 1: List memory tests and find each twin by name/assertion**
```bash
grep -n 'async fn \|fn ' src/vector/memory.rs | sed -n '/mod tests/,$p'
grep -n 'async fn ' tests/integration_qdrant.rs
```
Audit-known twins: memory `~822/869` ↔ qdrant `~1362/1407`; `~891` ↔ `~1430/1453`; `~922-983` ↔ `~1506-1582`; `~1063` ↔ `~1701`; `~1080` ↔ `~1763`.
- [ ] **Step 2: Delete every memory test with a Qdrant twin. Keep tests with no twin** (list them in the commit message).
- [ ] **Step 3: `cargo test vector::memory` and, with Qdrant up, `cargo test --test integration_qdrant`** — green.
- [ ] **Step 4: Commit** — `chore(kiss): memory store keeps only the tests Qdrant does not already run`

### Task 9: `jobs/consolidate.rs` — drop tests that re-run dedupe through the sweep

**Files:**
- Modify: `src/jobs/consolidate.rs` (helper `disagreeing` + ~10 tests ~L1809-2183 and ~L2215-2295; helper `sweep_and_dedupe` ~L750-767)

- [ ] **Step 1: Confirm each pair.** For each candidate consolidate test, find the `dedupe.rs` test with the same assertion:
  - `the_pass_records_what_it_would_do_when_autonomy_is_off` ↔ dedupe `with_autonomy_off_a_duplicate_verdict_is_filed_as_would_merge`
  - `a_direction_naming_the_newer_artifact_is_not_trusted` ↔ dedupe `a_replacement_naming_the_newer_artifact_is_not_trusted`
  - `an_enabled_dedupe_pass_marks_a_real_contradiction` ↔ dedupe `a_value_conflict_is_escalated_and_never_merged`
  - `a_failed_dedupe_call_leaves_the_pair_pending` ↔ dedupe `a_failed_dedupe_leaves_the_component_pending`
  - `a_confident_direction_is_applied_once_autonomy_is_on` ↔ dedupe `an_applied_replacement_does_not_wait_for_an_operator`
  - plus the others in the same block that only differ by verdict.
- [ ] **Step 2: Delete those tests, `disagreeing`, and `sweep_and_dedupe` if now unused.** Keep the sweep-contract tests (~L2184-2214, ~L1531-1656, ~L2366+: arms units, no model call, budget/ordering).
- [ ] **Step 3: `cargo test jobs::consolidate jobs::dedupe`** — green.
- [ ] **Step 4: Commit** — `chore(kiss): dedupe verdicts are tested once, in dedupe`

### Task 10: `web/` test harness + markup assertions

**Files:**
- Create: `src/web/test_support.rs` (`#[cfg(test)]`)
- Modify: `src/web/mod.rs` (`#[cfg(test)] mod test_support;`), `src/web/ui.rs` tests (~L1657-3359), `src/web/api.rs` tests (~L866-1030 helpers; keep `mod patch_tests`), `src/web/assets.rs` tests (~L176-390)

- [ ] **Step 1: Inventory helpers**
```bash
grep -n 'fn ' src/web/ui.rs | sed -n '/cfg(test)/,$p' | head -40
grep -n 'fn ' src/web/api.rs | sed -n '/cfg(test)/,$p' | head -40
```
Known: ui `app_with_session, app_session_and_core, app_for, app_with_embedded_corpus, get, get_body, body_of, form, flat, urlencoding_of`; api `app_and_token, app_token_and_core, app_from_core, get, post_json, json_of, post_file, post_file_with, post_two_files`.
- [ ] **Step 2: Write `test_support.rs`** with one router builder and the request helpers, keeping the *union* of behaviours:
```rust
//! Shared harness for web tests.
use super::*; // adjust to what ui/api tests import today
pub async fn app_from_core(core: Core) -> (Router, Arc<AuthContext>) { /* body of api's app_from_core */ }
pub async fn app_and_token() -> (Router, String) { /* api's */ }
pub async fn app_token_and_core() -> (Router, String, Core) { /* api's */ }
pub async fn app_with_session() -> (Router, String /*cookie*/) { /* ui's */ }
pub async fn app_session_and_core() -> (Router, String, Core) { /* ui's */ }
pub async fn get(app: &Router, path: &str, auth: Auth) -> Response { /* Auth = enum { Bearer(String), Cookie(String), None } */ }
pub async fn body_of(resp: Response) -> String { … }
pub async fn json_of(resp: Response) -> serde_json::Value { … }
pub async fn post_json(app:&Router, path:&str, token:&str, body: serde_json::Value) -> Response { … }
pub async fn post_form(app:&Router, path:&str, cookie:&str, fields:&[(&str,&str)]) -> Response { … }
pub async fn post_file(app:&Router, path:&str, token:&str, parts:&[(&str,&str,&[u8])]) -> Response { /* covers post_file/post_file_with/post_two_files */ }
```
Copy the existing bodies verbatim; the point is one copy, not new behaviour.
- [ ] **Step 3: Rewrite `ui.rs`/`api.rs` tests to `use super::test_support::*;`** and delete the per-file helpers.
- [ ] **Step 4: Delete markup-string assertions in `ui.rs`.** Rule: any test whose *only* assertion is `body.contains("<…html…>")` goes; a test that also asserts a status, redirect, auth outcome, or absence of unsanitized input stays (delete only its markup lines if it has both). Keep the ~15 behaviour tests: 401/redirect on every route without auth, sanitisation of pasted HTML, 404 for unknown ids, PUT edit round-trip, capture form → corpus exists.
- [ ] **Step 5: `assets.rs`:** delete tests asserting file *contents* (fonts embedded, manifest fields, sw.js text, offline page colour, "no external requests" greps ~L176-193, 213-260, 284-311, 342-390). Keep the four testing `serve`: content-type, cache-control, private-prefix 404, path traversal. If `assets_router_standalone`/generic `routes<S>()` (~L90-106) existed only for the deleted tests, delete them too — verify with grep.
- [ ] **Step 6: `cargo test web::`** — green.
- [ ] **Step 7: Commit** — `chore(kiss): one web test harness; assert behaviour, not markup`

### Task 11: `core/search.rs`, `core/ingest.rs`, `core/mod.rs` test cleanup

**Files:**
- Modify: `src/core/search.rs` tests (~L541-1483), `src/core/ingest.rs` tests (~L1133-2310), `src/core/mod.rs` (~L216-330 `test_core_*`)

- [ ] **Step 1: search.rs** — delete `a_deprecated_artifact_is_not_a_neighbour` (~L1276, identical to `integration_qdrant.rs:~1806`); delete the four tests at ~L1120-1243 that only exercise `core.vectors.stale_candidates`/`set_last_verified_at` (covered in `integration_qdrant.rs:~184-200,1882,1978-1990,2095`); if `reembed_all` (~L582) and the bespoke `Embedder` impl (~L600-618) then have no users, delete them.
- [ ] **Step 2: ingest.rs** — merge the two PNG fixtures (`a_seeded_png` ~L1144, `a_png` ~L2148 → keep one, name `a_png`); delete `the_same_photo_twice_is_a_duplicate_before_any_model_call` (~L2209, same assertion as `a_duplicate_image_is_recognised_by_the_shared_hash` ~L1286); pull the shared `point()`/`one_artifact()` scaffolding of the five `heal_store_drift` tests (~L2013-2147) into one helper.
- [ ] **Step 3: mod.rs** — replace the nine `test_core_*` fns with `test_core()` plus a `TestCoreBuilder { synthesizer, embedder, reranker, completer, config_edit }` (or a `build(overrides: Overrides)` fn with `Default`), keeping the three most-used names (`test_core`, `test_core_with_config`, `test_core_counting_reranked_docs`) as thin aliases if they have >5 callers (`grep -rn test_core_ src | wc -l` per name). Delete the rest and update callers.
- [ ] **Step 4: `cargo test`** — green.
- [ ] **Step 5: Commit** — `chore(kiss): core tests: one fixture per shape, no duplicates of the Qdrant suite`

### Task 12: `infer/` and `jobs/describe.rs` test cleanup

**Files:**
- Modify: `src/infer/openai.rs` tests (~L919-1127), `src/infer/fake.rs` (~L658-743), `src/jobs/describe.rs` tests (~L276-380), `src/jobs/consolidate.rs` seed fixtures (~L777-921), `src/eval/export.rs` tests (~L74-230)

- [ ] **Step 1: openai.rs** — fold `embedder_asks_for_float_encoding_explicitly`, `embedder_sends_a_batch_and_orders_results_by_index`, `base_url_with_a_trailing_slash_does_not_double_up`, `completer_returns_message_content` into one `wire_format_is_what_we_document` test with one wiremock server asserting all four properties; fold `reranker_tei_style`/`reranker_cohere_style`/`reranker_drops_out_of_range_indexes` into one table-driven test over `[(RerankStyle, response_json, expected_order)]`.
- [ ] **Step 2: fake.rs** — delete `mod tests` (~L658-743): the fakes are exercised by ~200 other tests.
- [ ] **Step 3: describe.rs** — one `async fn image_corpus(core:&Core) -> Corpus` helper replacing the three copied `insert_image_corpus(NewImage{…})` blocks; delete `without_a_vision_role_the_job_waits_rather_than_failing_the_corpus` (strict subset of `without_a_vision_role_an_exhausted_job_keeps_waiting`).
- [ ] **Step 4: consolidate.rs seeds** — replace `seed_titled`/`seed`/`seed_into_new_corpus` with:
```rust
/// Seeds artifacts with vectors. `corpus`: None → the shared test corpus; Some(title) → a fresh corpus.
async fn seed(core:&Core, corpus: Option<&str>, rows:&[(Option<&str> /*title*/, &str /*text*/, [f32;2])]) -> Vec<String /*ids*/>
```
Update callers in `consolidate.rs`, `classify.rs`, `dedupe.rs`.
- [ ] **Step 5: export.rs** — keep one round-trip test (export → files exist → JSON parses → ids match) and one "no pairs → empty pairs.json" test; delete the rest.
- [ ] **Step 6: `cargo test`** — green.
- [ ] **Step 7: Commit** — `chore(kiss): fewer, table-driven infer tests; one fixture per job`

### Task 13: Tier 2 report
- [ ] `git diff --stat master..HEAD | tail -1`; test count before/after (`cargo test 2>&1 | grep 'test result'`).

---

## Tier 3 — internal dedupe (behaviour identical)

### Task 14: `infer/openai.rs` — one endpoint, one chat call

**Files:**
- Modify: `src/infer/openai.rs` (~L88-624: five structs, `chat`/`complete`/`describe` bodies, `HttpCompleter::new`/`for_judging` ~L447-481)

**Interfaces (produces):**
```rust
pub(crate) struct Endpoint { client: reqwest::Client, base_url: String, model: String, api_key: Option<String>, role: &'static str }
impl Endpoint { fn new(cfg: &RoleLike, role:&'static str, timeout: Duration) -> Self; fn url(&self, path:&str) -> String; async fn post_json(&self, path:&str, body:&serde_json::Value) -> Result<serde_json::Value> }
async fn chat(ep:&Endpoint, body: serde_json::Value) -> Result<ChatReply>  // ChatReply { content: String, finish_reason: Option<String>, prompt_tokens: Option<u64>, completion_tokens: Option<u64> }
```
- [ ] **Step 1: Read the five structs and the three chat bodies; note every field of the log lines** (ms, tokens, finish_reason).
- [ ] **Step 2: Add `Endpoint` and `chat()`.** `chat` posts to `chat/completions`, logs `tracing::info!(role, ms, prompt_tokens, completion_tokens, finish_reason)`, returns `choices[0].message.content` or `Error::Inference{role, "no message content"}`. Keep the existing 4xx-is-rejection / retryable classification exactly where it is today.
- [ ] **Step 3: Replace each struct's four fields with `ep: Endpoint`; make `HttpSynthesizer::chat`, `HttpCompleter::complete`, `HttpDescriber::describe` call `chat(&self.ep, body)`.** Fold `HttpCompleter::for_judging` into `new(cfg, role, schema)`; update the two callers in `src/core/mod.rs`/`main.rs`.
- [ ] **Step 4: `cargo test infer::openai`** — the wiremock tests are the behaviour lock; green.
- [ ] **Step 5: Commit** — `chore(kiss): one HTTP endpoint struct and one chat call under five roles`

### Task 15: `infer/fake.rs` — fewer doubles; `prompt.rs` salvage; `split` cleanups

**Files:**
- Modify: `src/infer/fake.rs` (`budget()` copies ~L179-191, 261-273, 359-371, 388-400; `LyingSpanSynthesizer`/`HallucinatingSynthesizer` ~L344-401; `EchoCompleter`/`FakeCompleter` ~L512-552; `ParaphrasingSynthesizer::recovering/persistent` ~L219-233)
- Modify: `src/infer/prompt.rs` (`salvage_truncated` ~L498-537, `parse_response` ~L607-640)
- Callers: `src/jobs/synthesize.rs:~1112,1139`, `src/core/ask.rs:~267`

- [ ] **Step 1: fake.rs** — `const FAKE_BUDGET: SynthesisBudget = …` used by all four; `LyingSpanSynthesizer`+`HallucinatingSynthesizer` → `struct MisreportingSynthesizer { echo_text: bool }`; `EchoCompleter`+`FakeCompleter` → `ScriptedCompleter` gains `ScriptedCompleter::echo()` and `ScriptedCompleter::fixed(s)`; `ParaphrasingSynthesizer::new(recovers: bool)`. Update the callers.
- [ ] **Step 2: prompt.rs** — delete `salvage_truncated`; `parse_response` tries `serde_json::from_str::<Envelope>` then `salvage_objects`. Run `cargo test infer::prompt` — `a_truncated_list_keeps_the_artifacts_that_finished` and `a_response_cut_before_any_chunk_closed_is_still_an_error` must still pass.
- [ ] **Step 3: `cargo test`** — green.
- [ ] **Step 4: Commit** — `chore(kiss): fewer fakes; one salvage path for a cut-off reply`

### Task 16: Fold `jobs/classify.rs` into `jobs/relate.rs`

**Files:**
- Delete: `src/jobs/classify.rs`
- Modify: `src/jobs/relate.rs`, `src/jobs/mod.rs` (`mod classify;`), `src/jobs/consolidate.rs:~565` (second caller — leave a compiling call now; Task 19 removes it)

- [ ] **Step 1: Move `classify_pair` and `contains_normalized` into `relate.rs` as `pub(super) async fn classify_pair(core:&Core, a:&Chunk, b:&Chunk, score:f32) -> Result<()>` (drop the `Verdict` return; callers ignore it or matched two variants — for the consolidate caller, replace the `Ok(Verdict::Queued|Contained)` bookkeeping with the equivalent side-effect check it needs, or simply call and continue if the outcome only fed the sampled scan's `Outcome.examined`).**
- [ ] **Step 2: Move classify's tests into `relate.rs`; delete `one_synthesis_call_emitting_a_passage_twice_resolves_itself` and `containment_across_two_corpora_is_left_alone` (identical to consolidate `~L2060-2124`).**
- [ ] **Step 3: `cargo test jobs::`** — green.
- [ ] **Step 4: Commit** — `chore(kiss): pair classification lives with its one producer`

### Task 17: Store setter helpers; dedupe attempt counter; small core/web folds

**Files:**
- Modify: `src/store/artifacts.rs` (~L543-592, 783-820, 926-957), `src/store/corpora.rs` (~L389-445, 599-639), `src/store/pairs.rs` (~L242-381 mutators; `unreadable_judgements` ~L25,483), `src/jobs/dedupe.rs` (~L182-184, 198, 227-230), `src/jobs/mod.rs` (~L83-101 `exhausted` arms), `src/store/mod.rs`
- Modify: `src/core/search.rs` (~L218-236, 259-277, 424-442 `SearchResult` mapping; ~L284-317 wrappers), `src/core/ingest.rs` (~L98-135 `Capture` builder; ~L159-167,200-210,297-305,358-365 duplicate outcome), `src/web/ui.rs` (`ArtifactDetail` ~L82-139, ~L1545-1568; `ArtifactView` ~L70-79), `src/web/corpus_view.rs` (~L33-101), templates `_artifact_detail.html`, `_artifact.html`
- Modify: `src/jobs/{classify→relate,merge,consolidate}.rs` — `try_supersede`

- [ ] **Step 1: Store — add in `src/store/mod.rs`:**
```rust
impl Store {
    /// One-column UPDATE, touching updated_at when the table has one.
    pub(crate) async fn set_field<V: sqlx::Type<Sqlite> + sqlx::Encode<'static, Sqlite> + Send + 'static>(&self, table:&'static str, col:&'static str, id:&str, v: V) -> Result<()> { /* format!("UPDATE {table} SET {col} = ?, updated_at = ? WHERE id = ?") — table/col are compile-time literals only */ }
}
```
Rewrite the 8-14-line setters as 1-3 lines calling it; collapse `set_x`/`clear_x` pairs (`corpus_coverage`, `described_text`, `artifact_flags`) into one fn taking `Option<_>`. Public method names that callers use stay unless a pair merges (update callers).
- [ ] **Step 2: Dedupe counters — stop incrementing/reading `unreadable_judgements` and `MAX_UNREADABLE_JUDGEMENTS`; the pending-pairs query orders and gates on `judge_attempts` only; the prompt nonce keeps using `judge_attempts`. Column stays in `schema.sql`.** Merge the two identical `Stage::Dedupe if exhausted` / `Stage::Title if exhausted` arms in `jobs/mod.rs` into one pattern.
- [ ] **Step 3: `try_supersede`** in `src/jobs/mod.rs`:
```rust
/// Supersede `loser` by `winner`; a failure is logged and swallowed because every caller carries on regardless.
pub(crate) async fn try_supersede(core:&Core, loser:&str, winner:&str, why:&str) -> bool
```
Replace the four copies (relate/former classify ~L93-101, merge ~L80-83 & ~L104-107, consolidate ~L488-496).
- [ ] **Step 4: search.rs** — `impl From<SearchHit> for SearchResult` (with `weak: false`); `search_inner` sets `weak` after; delete `search_timed`/`search_capped`, keep `pub async fn search(&self, q:&SearchQuery, cap:Option<usize>) -> Result<(Vec<SearchResult>, SearchTiming)>`; update `web/api.rs`, `web/ui.rs`, `mcp/mod.rs`, `core/ask.rs`, `tests/eval.rs`.
- [ ] **Step 5: ingest.rs** — `impl IngestOutcome { fn existing(c:&Corpus) -> Self }`; `Capture` becomes `#[derive(Default)]` plain struct, callers in `web/api.rs` use a literal with `..Default::default()`; delete the `with_*` methods.
- [ ] **Step 6: ui.rs** — `struct ArtifactDetail { pub c: Chunk, html, sources, merged, orphaned_source, corpus_restored, source_at_lines, slice_label, slice_lines, terms, related }` (adjust field list to what the template uses); templates read `d.c.title` etc. Same for `ArtifactView` if it copies from `Chunk`. `corpus_view.rs`: replace trait + two impls with `pub fn slice(source:&CorpusSource, span:&Span, context:usize) -> CorpusSlice` and a `match source.origin { Origin::Image => "transcript lines", _ => "lines" }` for the label.
- [ ] **Step 7: `cargo test`** — green (askama build catches template mismatches).
- [ ] **Step 8: Commit** — `chore(kiss): one setter helper, one attempt counter, one shape per value`

### Task 18: Comment prose trim

**Files:**
- Modify: `src/jobs/consolidate.rs` (~L1-35, 124-138, 604-621, 642-735), `src/jobs/merge.rs`, `src/jobs/relate.rs`, `src/jobs/reconcile.rs`, `src/jobs/embed.rs` (~L600-656 `payload_of`), `src/jobs/synthesize.rs` (~L158-197), `src/jobs/mod.rs` (~L79-155)

- [ ] **Step 1: Rule.** A comment survives if it states an invariant a reader could violate without noticing (why, not what) — keep verbatim: rearm-vs-enqueue (`consolidate`), "complete after, never before" (`jobs/mod.rs:~63-66`), merge write ordering (`merge.rs:~20-33`), prompt-cache prefix order (`prompt.rs`). Delete: incident narration ("used to X, which broke Y"), restatements of ROADMAP.md, paragraphs restating the next five lines of code. Module headers ≤ 6 lines.
- [ ] **Step 2: Apply file by file; `cargo build` (doc comments on pub items must still parse).**
- [ ] **Step 3: Commit** — `chore(kiss): comments state invariants, not history`

### Task 19: Tier 3 report
- [ ] `git diff --stat master..HEAD | tail -1`; `cargo clippy` warnings vs baseline; `cargo test` green.

---

## Tier 4 — approved machinery

### Task 20: Sampled `near_pairs` scan → relate backstop (spec 4a)

**Files:**
- Modify: `src/jobs/consolidate.rs` (~L394-401 call + `Outcome.examined`; ~L403-430 `from_sweep`; ~L545-576 review-band `classify_pair` loop), `src/vector/qdrant.rs` (~L1881-1979 `near_pairs`), `src/vector/memory.rs` (~L430-473 + tests), `src/vector/mod.rs` (`NearPair` ~L145-152, ~L223-232, trait method ~L379-384), `src/config.rs` (`consolidate.sample` + retired-key warning), `config.example.toml`, `src/store/artifacts.rs` (new query), `tests/integration_qdrant.rs` (near_pairs tests)

**Interfaces (produces):**
```rust
// store/artifacts.rs
/// Active artifacts that have no row in artifact_pairs and no live Relate job — the backstop for a Relate that never got armed.
pub async fn list_unrelated_artifact_ids(&self, limit: usize) -> Result<Vec<String>>
```
- [ ] **Step 1: Write the failing test** in `consolidate.rs`:
```rust
#[tokio::test]
async fn the_sweep_arms_relate_for_an_indexed_artifact_that_was_never_related() {
    let core = test_core().await;
    let ids = seed(&core, None, &[(None, "alpha", [1.0, 0.0])]).await; // active + embedded, no pair rows, no relate job
    run(&core).await.unwrap();
    assert!(core.store.has_live_job(Stage::Relate, &ids[0]).await.unwrap()); // use whatever "is a job queued for this target" query exists
}
```
- [ ] **Step 2: Run** — fails.
- [ ] **Step 3: Implement `list_unrelated_artifact_ids`** (SQL: `SELECT a.id FROM artifacts a WHERE a.status='active' AND NOT EXISTS (SELECT 1 FROM artifact_pairs p WHERE p.a_id=a.id OR p.b_id=a.id) AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.stage='relate' AND j.target_id=a.id AND j.completed_at IS NULL) LIMIT ?` — adapt column names to `schema.sql`), and in `consolidate::run` replace the `near_pairs` call + `from_sweep` clustering + review-band loop with: for each id → `core.store.enqueue(Stage::Relate, id)`. Union-find input is now only `pairs_by_state(NearIdentical, …)`.
- [ ] **Step 4: Delete `VectorStore::near_pairs`, `NearPair`, both impls, their tests (memory + integration_qdrant), `consolidate.sample` (+ example.toml lines), `Outcome.examined`; add `consolidate.sample` to the retired-key warning.**
- [ ] **Step 5: Re-point remaining consolidate tests that relied on the sample scan at `relate::run` (call `relate::run(&core, id)` explicitly before `run(&core)`).**
- [ ] **Step 6: `cargo test` + `cargo test --test integration_qdrant`** — green.
- [ ] **Step 7: Commit** — `chore(kiss): relate is the one duplicate detector; the sweep only backstops its arming`

### Task 21: Delete `full_lifecycle_reconcile` scan (spec 4b)

**Files:**
- Modify: `src/jobs/consolidate.rs` (~L203-302, call ~L346-351, `DRIFT_SCAN`, tests ~L1287-1321, 1402-1482), `src/core/ingest.rs:~714` (caller), `src/store/artifacts.rs:~821` (`list_non_active_artifacts`), `src/vector/mod.rs:~323`, `src/vector/qdrant.rs:~1281`, `src/vector/memory.rs:~194` (`non_active_ids`), `tests/integration_qdrant.rs`

- [ ] **Step 1: grep** `full_lifecycle_reconcile\|non_active_ids\|list_non_active_artifacts\|DRIFT_SCAN` — delete every hit. `repair_lifecycle_drift` (marker pass, ~L180-201) stays.
- [ ] **Step 2: `cargo test` + integration** — green.
- [ ] **Step 3: Commit** — `chore(kiss): lifecycle drift is repaired from the marker alone`

### Task 22: `heal_store_drift` → warn (spec 4c)

**Files:**
- Modify: `src/core/ingest.rs` (~L760-905), `src/store/artifacts.rs` (`RestoredArtifact` ~L190-210, `restore_artifact` ~L418-439, `list_all_artifact_ids`/`list_embedded_artifact_ids` ~L833-868 — keep any with another caller), `src/store/corpora.rs` (`ensure_restored_corpus` ~L312-331), `src/vector/mod.rs` (`payloads_of`, `all_artifact_ids`, `VectorPayload.provenance` ~L64), `src/vector/qdrant.rs` (~L1312-1350, ~L1398-1451), `src/vector/memory.rs`, `src/web/ui.rs:~831,1563` (restored badge) + template, `src/main.rs`, `src/jobs/consolidate.rs`, tests in `ingest.rs` (~L2018-2150) and `integration_qdrant.rs` (~L1930-2044)

- [ ] **Step 1: New body:**
```rust
/// Counts artifacts that Qdrant knows and SQLite does not, and warns. Repair is a restore of both stores together (see ROADMAP).
pub async fn report_store_drift(&self) -> Result<()> {
    let known: HashSet<String> = self.store.list_all_artifact_ids().await?.into_iter().collect();
    let in_qdrant = self.vectors.count_ids_not_in(&known).await?; // if no such trait method exists, keep `all_artifact_ids` for this one use and diff locally
    if in_qdrant > 0 { tracing::warn!(missing = in_qdrant, "vector store holds artifacts SQLite does not; restore both stores from the same snapshot"); }
    Ok(())
}
```
Prefer keeping *one* trait method (`all_artifact_ids`) over adding a new one; delete `payloads_of`, `restore_artifact`, `RestoredArtifact`, `ensure_restored_corpus`, `VectorPayload.provenance` (stop writing it; existing points keep it harmlessly), the badge, and the restore tests. Add one test: seeded point without a row → no row is created and the call returns Ok.
- [ ] **Step 2: `cargo test` + integration** — green.
- [ ] **Step 3: Commit** — `chore(kiss): store drift is reported, not repaired from payloads`

### Task 23: Remove `consolidate.autonomous` and `WouldMerge` (spec 4d)

**Files:**
- Modify: `src/config.rs` (~L152-159 + retired-key warning), `src/jobs/dedupe.rs` (~L387-438 two-branch `Replaced` arm, ~L446-458 `WouldMerge` early return), `src/jobs/consolidate.rs` (~L630-640 `reopen_would_merge_pairs` + tests ~L1561-1597), `src/store/pairs.rs` (`WouldMerge` variant, `reopen_would_merge_pairs`), `src/store/mod.rs` (migration), `docs/superpowers/specs/2026-08-14-autonomous-consolidation-design.md`, `config.example.toml`, README/ROADMAP mentions

- [ ] **Step 1: Failing test** in `store/mod.rs` tests: open a DB with a `would_merge` pair row → after `migrate()` its state is `pending`.
- [ ] **Step 2: Add to the migration sequence in `Store::migrate` (next to the `DELETE FROM jobs WHERE stage='judge'` step):** `UPDATE artifact_pairs SET state='pending' WHERE state='would_merge'`. Delete `PairState::WouldMerge` (keep `parse` tolerant: unknown → error as today, since the row was just migrated). Delete `reopen_would_merge_pairs` in store and jobs, the `autonomous` field, the `WouldMerge` early return, and the "propose only" branch of the `Replaced` arm — always act. Add `consolidate.autonomous` to the retired-key warning.
- [ ] **Step 3: Delete the tests** `with_autonomy_off_a_duplicate_verdict_is_filed_as_would_merge`, `a_replacement_is_only_proposed_while_autonomy_is_off`, and the consolidate re-arm tests. Trim the would-merge/autonomous section of the 2026-08-14 spec to one sentence ("Every verdict is acted on; every merge and supersede has undo."). Update README/ROADMAP/config.example.toml lines mentioning `autonomous`.
- [ ] **Step 4: `cargo test`** — green.
- [ ] **Step 5: Commit** — `chore(kiss): duplicate verdicts are acted on; undo is the review`

### Task 24: Drop `tokenizers` and the gate circuit breaker (spec 4e, 4f)

**Files:**
- Modify: `Cargo.toml` (`tokenizers`), `src/infer/budget.rs` (~L9,16-38,42-48 `TokenCounter::Exact`), `src/config.rs` (`tokenizer_path`, `pacing.breaker_after`, `pacing.breaker_probe_secs` + retired-key warning), `config.example.toml` (~L278-279), README.md:~180, `src/infer/gate.rs` (~L41-43, 54-68, 100-118 breaker arm of `ready_at`, 138-159; tests ~L366-432, 452-468; also `call_refused`/`succeeded`/`failed` → one `finished(Option<&Error>)`)

- [ ] **Step 1: budget.rs** — `TokenCounter` becomes the estimate only (`chars * 2 / 7` as today); delete the `Exact` variant, `tokenizer_path`, the dep. `cargo build` must not pull `onig_sys` (`cargo tree | grep -c onig` → 0).
- [ ] **Step 2: gate.rs** — delete the breaker state, its config, its `ready_at` arm and tests; keep semaphore, cooldown, interactive lease and their tests. Collapse `BackgroundPermit::{succeeded,failed,refused}` into `finished(&self, outcome: Option<&Error>)` — cooldown stamping unchanged. Update the callers in `jobs/` and `core/`.
- [ ] **Step 3: Retired-key warning gains `tokenizer_path`, `pacing.breaker_after`, `pacing.breaker_probe_secs`.**
- [ ] **Step 4: `cargo test`** — green.
- [ ] **Step 5: Commit** — `chore(kiss): estimate tokens; the queue's backoff is the circuit breaker`

### Task 25 (opt-out): Ops staleness/lifecycle section (spec 4g)

*Skip this task if the owner says the Ops section stays.*

**Files:**
- Modify: `src/web/ui.rs` (`DeprecatedRow`/`StaleRow` ~L201-212; loops in `ops()` ~L1163-1200; `deprecate_ui`/`reactivate_ui`/`verify_ui` ~L1398-1427; `ReturnTo` ~L1289-1305; router entries), `src/web/templates/ops.html` (~L54-113), related ui tests
- Keep: `GET /api/v1/consolidation/stale` (a route), the same actions on `_artifact_detail.html`

- [ ] **Step 1: Confirm the artifact detail page still offers deprecate/reactivate/verify** (`grep -n 'deprecate\|reactivate\|verify' src/web/templates/_artifact_detail.html`). If those buttons post to the `_ui` routes being deleted, keep the three route handlers and delete only the Ops rows, loops, template section and `ReturnTo` (redirect back to the artifact page unconditionally).
- [ ] **Step 2: Delete; `cargo test`** — green.
- [ ] **Step 3: Commit** — `chore(kiss): staleness actions live on the artifact, not on Ops`

### Task 26: Final report
- [ ] `cargo build --release && cargo test && cargo test --test integration_qdrant && cargo clippy`.
- [ ] Line counts vs Task 0 baseline (Rust total, test lines, docs); `cargo tree | wc -l` before/after.
- [ ] Delete this plan file (`docs/superpowers/plans/2026-08-15-kiss-shrink.md`) in the last commit — it is finished work, per Tier 0's own rule — and note it in the PR body.
- [ ] Hand off with `superpowers:finishing-a-development-branch`.
