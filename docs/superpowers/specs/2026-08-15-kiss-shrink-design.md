# KISS shrink — design

Goal: make the codebase smaller without removing anything that is wired and
working. Approved 2026-08-15.

## Principles

1. **Every wired feature stays.** All routes (incl. `PATCH /artifacts`,
   `/resurface`, `/consolidation`, the URL-fetch door), search-event `Fold`
   coalescing, ask, all three auth modes, MCP tools, the extension.
2. **Every in-place upgrade path stays.** A deployed instance must upgrade
   without a wipe: Qdrant alias generations + `--reindex`, `ADDED_COLUMNS`
   migrator, dual `superseded`/`status` payload flag, lifecycle backfill,
   `--backfill-lifecycle`, retired-config-key carry all remain. Columns that
   stop being used are left in the schema; no `DROP COLUMN`.
3. **Removed config keys warn.** `consolidate.autonomous`, `consolidate.sample`,
   `pacing.breaker_after`, `pacing.breaker_probe_secs`, `tokenizer_path` get
   one line in the existing moved/retired-key warning so an old config file
   is not silently misread.
4. Deleted code lives in git history. No "removed X" comments.

## Tiers (each compiles and passes `cargo test` on its own; stop after any)

### Tier 0 — docs (~21k lines)
- Delete `docs/superpowers/plans/*` (11 files) and `spec.md` (stale vs README).
- Delete 6 of 7 specs; keep `2026-08-14-autonomous-consolidation-design.md`
  (cited by ROADMAP) and trim its would-merge/autonomous section after Tier 4.

### Tier 1 — dead code + two bug fixes (~150 lines)
Delete: `FakeSynthesizer::failing_on`, `verify::span_is_plausible`,
`Error::Duplicate`, `split::is_heading_for_test` (+ its one assertion),
`StoreDrift` counters (log line stays), `src/bin/eval_prepare.rs` + Cargo
target + the three error strings in `tests/eval.rs` naming it.
Fix: the `consolidate.enabled` gate stops capture repair (see the correction
below); the hard-coded `theme: "light"` field and `data-theme` attribute go so
`prefers-color-scheme` applies.

**Corrected after review (PR #16).** Three Tier 1 items were wrong and did not
ship as designed:

- `openai::probe` was listed as uncalled. It is called, four times, by
  `startup_checks`, and it is the only boot-time report of an unreachable
  inference endpoint. It stays.
- The EXIF `tags` catch-all was listed as never read. Nothing read it *back*
  because nothing displayed it — but ingest is the only moment those tags
  exist, the original file is never stored, and deleting the catch-all deleted
  them permanently. It stays, and the corpus page now shows it.
- Moving `reconcile::run` above the `enabled` early return in
  `consolidate::run` fixes nothing: that function is only reached through a
  `Consolidate` job, and `spawn_consolidation_ticker` returns before its loop
  when `enabled` is false, so no such job is ever queued. The four passes that
  are capture repair rather than duplicate hygiene — reconcile, dangling
  supersessions, torn lifecycle writes, store drift — moved to their own
  hourly ticker behind no setting. Merge repair stays on the sweep, which is
  the only thing that creates merges.
Left alone (touch a deployed DB): `Stage::Enrich` alias arm, `query_vec` /
`vec_dim` / `via_id` write-only columns, `main.rs` upgrade flags.

### Tier 2 — tests only (~3k lines, no impl change)
- `vector/memory.rs` `mod tests`: delete tests with a twin in
  `tests/integration_qdrant.rs`; keep any without one.
- `jobs/consolidate.rs`: delete tests that re-run `dedupe.rs` verdict tests
  through the sweep, and the `sweep_and_dedupe` helper.
- `web/ui.rs`: delete exact-HTML-string assertions; keep auth/sanitize/404/
  redirect tests. Merge the `ui.rs` and `api.rs` harnesses into one
  `web::test_support`.
- `web/assets.rs`: delete file-content assertions; keep the four `serve` tests.
- `core/search.rs`, `core/ingest.rs`: delete duplicated tests and the second
  PNG fixture.
- `infer/openai.rs`: fold single-field wiremock tests into table-driven ones.
- `infer/fake.rs`: delete tests of the fakes themselves.
- `jobs/describe.rs`, `jobs/consolidate.rs`: one seed/fixture helper each.
- `core/mod.rs`: nine `test_core_*` ctors → `build` with overrides.

### Tier 3 — internal dedupe, behaviour identical (~900 impl lines)
- `infer/openai.rs`: one `Endpoint{client,base_url,model,api_key,role}` and
  one `chat()` under the five role structs; `for_judging` folds into `new`.
- `infer/fake.rs`: shared `FAKE_BUDGET`; bool-parametrised variants
  (Lying/Hallucinating, recovering/persistent); Echo/Fake completers →
  `ScriptedCompleter`.
- `jobs/classify.rs` folds into `jobs/relate.rs` as a private fn; `Verdict`
  enum goes.
- Store: `set_x`/`clear_x` pairs → one `Option` argument; a `set_field`
  helper for one-column UPDATEs (artifacts, corpora, pairs).
- `web/ui.rs`: `ArtifactDetail` wraps `Chunk` instead of copying 12 fields.
- `web/corpus_view.rs`: trait → one fn with a `match` on origin.
- `core/search.rs`: `From<SearchHit> for SearchResult`; `search`/
  `search_timed`/`search_capped` → one entry point.
- `core/ingest.rs`: `IngestOutcome::existing()`; `Capture` builder → struct
  literal.
- `infer/prompt.rs`: `salvage_truncated` folds into `salvage_objects`.
- `jobs/mod.rs`: merge the two identical `exhausted` arms.
- One `try_supersede` helper replacing four "supersede, warn, carry on" copies.
- Dedupe attempt counters: keep `judge_attempts` only (columns stay).
- Comment prose: incident narration and ROADMAP restatements → one line;
  keep load-bearing invariants (rearm-vs-enqueue, complete-after-never-before,
  merge write ordering, prompt-cache prefix order).

### Tier 4 — approved machinery
a. Sampled `near_pairs` detection scan → a query arming `Stage::Relate` for
   active artifacts with no pair rows and no live relate job. `NearPair`, the
   trait method, both impls, the review-band loop and `consolidate.sample` go.
b. `full_lifecycle_reconcile(_scanning)`, `DRIFT_SCAN`,
   `list_non_active_artifacts`, `VectorStore::non_active_ids` go.
c. `heal_store_drift` → count + `warn!`. `RestoredArtifact`,
   `restore_artifact`, `ensure_restored_corpus`, `payloads_of`,
   `all_artifact_ids`, `VectorPayload.provenance` go; the "restored" badge and
   the `restored_at` column stay, because a deployed base may already hold
   placeholder corpora.

   *This is the one change here that can cost data an operator still has.* An
   artifact whose SQLite row is lost is no longer recoverable: the vector-store
   payload still holds its text, title, tags and lifecycle stamps, and nothing
   reads them back. The warning is the whole recovery story, and restoring both
   stores from the same snapshot is the supported answer.
d. `consolidate.autonomous`, `PairState::WouldMerge`, `reopen_would_merge_pairs`
   go; a one-line migration flips existing `would_merge` rows to `pending`.
e. `tokenizers` dep, `TokenCounter::Exact`, `tokenizer_path` go.
f. Gate circuit breaker and its two config keys go; semaphore, cooldown and
   interactive lease stay.
g. *Opt-out:* Ops staleness/lifecycle section (`DeprecatedRow`, `StaleRow`,
   `deprecate_ui`/`reactivate_ui`/`verify_ui`, `ReturnTo`, `ops.html`
   sections, `GET /api/v1/consolidation/stale` stays as it is a route).
   The same actions remain on artifact detail.

## Verification
Per tier: `cargo build`, `cargo test`, `cargo clippy`. Tiers 2 and 4
additionally run `tests/integration_qdrant.rs` against the docker-compose
Qdrant. Report line-count delta per tier. Branch `chore/kiss`, one commit
per tier.

## Out of scope
All routes, fetch door, `Fold`, ask, `QueryCache`, `[capture]` config knobs,
`RerankStyle` variants, OIDC `groups` claim, `Stage::Enrich`, migration /
alias / backfill machinery, `--recompute-coverage`, `--replace-legacy`.

## Expected result
Repo ~70k → ~40k lines; Rust ~48k → ~39k (≈−2.2k impl, ≈−6k test); the
`tokenizers`/`onig` build dependency gone; two bugs fixed.
