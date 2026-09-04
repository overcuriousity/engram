# Self-tuning: where things stand, for whoever plans stage 3

Written 2026-09-05, at the end of stage 2b. Read this before the spec's Part 5;
it says what exists, what was decided along the way that the spec does not,
and what stage 3 has to build on.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md`. The three
decisions in "The three decisions this rests on" are settled. Part 5 (earned
autonomy over the corpus jobs) is the only part with no plan.

**Branch:** `feat/observations`, off `master`. Stages 1, 2 and 2b are on it,
unmerged. It compiles and passes on its own; nothing on it depends on anything
not on it. Integration into `master` is the operator's call.

**Gate, every task:** `cargo fmt --all --check && cargo clippy --all-targets
--locked -- -D warnings && cargo test --locked`. Baseline at the end of 2b:
2530 passing, 0 failing, 1 ignored (the Qdrant integration test).

## What exists

| Piece | Where | Since |
|---|---|---|
| Generations: one live row per tenant, `params` as JSON, `run_id`/`predicted`/`state`, adopt / revert / tried / history | `src/store/generations.rs` | stage 1, 2 |
| Boot rule: which of file and live generation serves, and when the file wins | `boot_generation` in `src/store/generations.rs`, called from `tenants::generation_check` | stage 2 |
| Observations: cited, opened, unsupported, gave-up, with strengths | `src/store/observations.rs`; writers in `store/asks.rs`, `store/feedback.rs`, `jobs/observe.rs` | stage 1 |
| The verdict-paid sweep, its gate `recommend`, the chooser `candidates`, shared scoring `score` | `src/eval/sweep.rs` | pre-existing; 2, 2b |
| Lived record and the watch gate `holds_up` / `settled` | `src/eval/lived.rs` | stage 2 |
| The anchor: agreement with verdicts, `trustworthy` | `src/eval/anchor.rs` | stage 2 |
| The idle pass: adopt, watch, revert, suspend, stop-on-return | `src/jobs/tune.rs`, hung off `jobs/retention.rs` | stage 2, 2b |
| Four knobs on `RankingParams`, threaded through one pipeline | `src/core/ranking.rs`, `core/search.rs`, `vector/mod.rs` (`Recency`) | 2b |
| Ops disclosure: suspension first, live generation, standing, history | `src/web/insights.rs`, `templates/_evolve.html` | stage 2 |
| Config: `[evolve]` `give_up_window_secs`, `feed_sweep`, `autonomous`, `idle_secs`; `[vector] candidate_multiplier` | `src/config.rs`, `config.example.toml` | 1, 2, 2b |

`evolve.autonomous` and `evolve.feed_sweep` ship `false`.

## Decisions made on the way that the spec does not record

Each was raised as a plan deviation at the time and is in the plan files; they
are collected here because Part 5 leans on several of them.

- **The live generation serves after a restart** unless the file changed since
  the last boot or autonomy is off, in which case the file wins and is
  journaled as a hand-set generation. `meta.evolve.file_params` remembers what
  the file said last boot. The Apply button also journals a generation. Without
  this, serving and the journal diverged silently.
- **The watch ends** when lived evidence separates the two generations or the
  new one has as many observations as the old (`lived::settled`). As the stage 2
  plan was written it never ended, and the base would have adopted once.
- **`tried_candidates` keys on the live models**, not a timestamp. Corpus
  growth as re-eligibility is not implemented; it needs a size stamp on the
  reverted generation and a definition of "substantially" nobody has given.
- **The pass measures with the reranker off** and embeds nothing (stored query
  vectors seed the cache). Where a reranker serves search, the pass measures
  the pre-rerank order. Flagged, accepted.
- **Rerank on/off, `prime_lift` and spread are not tuned**, for reasons in the
  2b plan's "What the code admits". Rerank is a judgement: the gate scores
  ranking quality and cannot weigh the per-search model call adopting rerank
  would impose. `prime_lift` is blocked on a fact: observations do not record
  the sitting, and a column on `observations` is a recreated database.
- **The pass budget covers every rung on every axis** (16), not the nearest
  step: a tie keeps the current value, so an improvement behind a rung that
  ties would never be reached. This corrected the 2b plan mid-execution.
- **The anchor rule:** trustworthy unless disagreements are at least two and
  at least equal agreements. Suspension adopts nothing, reverts nothing, keeps
  recording, and is the first thing Insights says.
- **The pass runs at the retention unit's cadence** (`feedback.sweep_hours`,
  six hours by default, with the empty-run backoff), behind a quiet check of
  `evolve.idle_secs`. Not on its own ticker.
- **`--print-config` does not name the live generation.** Stage 1 dropped that
  task: the flag is per instance and generations are per tenant. Insights is
  the disclosure.

## What stage 3 has to build on, and what it does not have yet

The spec's Part 5 says merge, promote, consolidate, dedupe, reap and judgement
start by proposing and earn the right to act. Read against the tree:

- **A proposal record does not exist.** Nothing in stages 1–2b journals what a
  corpus job would have done. `eval_runs` is the ranking journal and does not
  fit: its rows are parameter pairs with metrics. Stage 3 needs its own table
  — new tables are free under the schema doctrine — holding job, lane,
  proposed action, confidence, and the operator's response (applied, rejected,
  left standing), plus whether an auto-applied action was later undone.
- **The bands exist for one job.** `[consolidate]` has `review_min` and
  `auto_supersede`, and `Config::normalize` refuses a configuration with no
  band between them. The other jobs have thresholds but not two lanes. Part 5
  grants autonomy per lane, so each job that joins needs its lanes named.
- **The undo paths exist as data, not all as paths.** Merges keep their
  originals; promotion has its undo; consolidation is reversible by design.
  Reap's graveyard table is read by nothing. The spec is explicit: reap may not
  become autonomous until a restore path exists a person can take. Build the
  restore before the proposal, or leave reap out.
- **The gate shape is available.** `recommend` (two net better, no aggregate
  loss), `holds_up` / `settled` (rates with one-observation noise), and
  `trustworthy` (at-or-below chance on two or more disagreements) are all
  "reads the evidence in front of it" rules with no tuned constant. Agreement
  rate per job and per lane wants the same shape: grant when agreements clear
  disagreements by more than one decision could account for, over enough
  decisions that one could not; revoke on the same rule pointed the other way.
  Do not write a floor of twenty.
- **The anchor is about ranking evidence, not about corpus jobs.** Part 5's
  "agreement" is a different quantity from `eval::anchor::agreement`: one
  compares observations with verdicts, the other compares proposals with
  responses. Same word, two tables. Name them apart.
- **The `can_judge` grant is the gate on proposals**, as Part 5 says. `CanJudge`
  in `src/web/tenant.rs` is what the Apply button already sits behind.

## Traps, so they are not rediscovered

From the stage 1 and 2 prompts and this session:

- `gen` is a reserved keyword in edition 2024.
- `crate::error::Error` has no `From<serde_json::Error>`; use the local
  `json()` / `from_json()` helpers in each store module.
- sqlx 0.9 refuses `sqlx::query(&format!(..))` — SQL is written as literals.
- SQLite `SUM` over zero rows of a REAL column decodes as INTEGER; `CAST(... AS
  REAL)`.
- Ids are ULIDs; two minted in one sitting share their head. Shorten by the
  tail.
- `search_candidates.rank` is 0-based; `ask_citations.n` and `observations.rank`
  are 1-based.
- Adding a field to `Core`, `Config` or `RankingParams` breaks initializers
  `cargo test --lib` does not compile, including `tests/eval.rs`. Always finish
  with `cargo clippy --all-targets`.
- Adding a column to an existing table is a recreated database. New tables are
  free. JSON columns widen with `#[serde(default = ...)]`.
- The in-memory vector store ignores recency entirely. Tests on the recency
  axes can assert structure, not ranking.
- `cargo fmt` reflows test code; an exact-string edit made after it may
  silently miss.
- Test names are sentences stating the rule.

## Plans, in order

1. `2026-09-04-self-tuning-stage-1.md` — built.
2. `2026-09-04-self-tuning-stage-2.md` — built, with the deviations above.
3. `2026-09-05-self-tuning-stage-2b.md` — built, with the budget correction.
4. Stage 3 — not planned. Start with brainstorming over Part 5 and this file;
   the open questions are the proposal table's shape, which jobs join first
   (consolidate has its lanes already), and whether reap's restore path is in
   scope.
