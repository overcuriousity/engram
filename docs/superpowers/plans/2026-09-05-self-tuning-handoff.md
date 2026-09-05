# Self-tuning: where things stand, for whoever plans stage 3b

Written 2026-09-05, at the end of stage 2b; updated the same day at the end of
stage 3a. It says what exists, what was decided along the way that the spec
does not, and what the next stage has to build on.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md`, with its
Part 5 replaced by `docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md`.
The three decisions in "The three decisions this rests on" are settled; the
second is restated in the stage 3 spec. Stage 3 is three plans: 3a (the three
knobs) is built; 3b (the corpus journal and two rules) and 3c (`review_min` on
the ladder) are not planned.

**Branch:** `feat/observations`, off `master`. Stages 1, 2, 2b and 3a are on it,
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
| Seven knobs on `RankingParams`: four from 2b; prime lift on the ladder, rerank as a flip against the served rank, spread on a lived rule | `src/core/ranking.rs`, `core/search.rs`, `vector/mod.rs` (`Recency`), `eval/sweep.rs` (`rerank_flip`), `jobs/tune.rs` (`next_spread`) | 2b, 3a |
| What priming read, per search; which pool rows were the band; which search an observation came from | `search_context`, `search_candidates.band`, `observations.event_id` in `src/store/schema.sql`; `Priming` in `core/search.rs` | 3a |
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
- **Rerank, `prime_lift` and spread are tuned as of 3a.** The flip is asked
  after the ladder, not among its candidates, because its base is the served
  rank and its MRR is not comparable with the ladder rows'. The band is not
  replayed; the spread rule is lived. Priming is captured at every door that
  primes, whatever the lift. See the 3a plan's "What this plan admits".
- **The band is in the pool, flagged.** An open on an appended hit is an
  observation; every reader that learns from the pool — the associate job's
  co-appearance read, the pursuit's shown list, `dealable!`, the hit-rank
  join — excludes `band = 1`, which is what keeps the Hebbian loop closed. A
  new reader of `search_candidates` has to decide which side it is on.
- **An open on an artifact that is both unshown in the ranked pool and shown
  in the band records the band rank**: the row the person saw.
- **The Apply button writes `associate.prime_lift` and `associate.spread_max`**
  beside the four `[vector]` keys. Rerank has no file key; the file says "on"
  by naming a reranker.
- **The pass budget covers every rung on every axis** (19 after 3a), not the
  nearest step: a tie keeps the current value, so an improvement behind a rung
  that ties would never be reached. This corrected the 2b plan mid-execution.
- **The anchor rule:** trustworthy unless disagreements are at least two and
  at least equal agreements. Suspension adopts nothing, reverts nothing, keeps
  recording, and is the first thing Insights says.
- **The pass runs at the retention unit's cadence** (`feedback.sweep_hours`,
  six hours by default, with the empty-run backoff), behind a quiet check of
  `evolve.idle_secs`. Not on its own ticker.
- **`--print-config` does not name the live generation.** Stage 1 dropped that
  task: the flag is per instance and generations are per tenant. Insights is
  the disclosure.

## What stage 3b has to build on, and what it does not have yet

Written against the original Part 5, which the stage 3 spec has since
replaced: the corpus jobs stay autonomous and answer to observations, not to
operator responses. Most of the facts below still hold and are what 3b reads;
the framing of "proposals" and "lanes" does not. Two corrections found while
brainstorming stage 3: reap's restore path exists (`Core::reactivate` exhumes
from the graveyard, over `POST /ui/ops/artifacts/{id}/reactivate`) and only a
listing is missing; and the pair review queue is not behind `CanJudge`, only
the tuning Apply button is.

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
4. `2026-09-05-self-tuning-stage-3a.md` — built, with the admissions at its
   top.
5. Stage 3b — not planned. Part B of the stage 3 spec: the `corpus_actions`
   journal, rule 1 (a survivor must still be found) and rule 2 (a give-up a
   hidden artifact would have answered), the graveyard listing, the
   disclosure. Note that the graveyard needs its vector kept (`vec BLOB`,
   `embed_model`), and that reap deletes the point today.
6. Stage 3c — not planned. Part C: `review_min` on the ladder, carried by the
   generation, read by `relate.rs` off `Core`.
