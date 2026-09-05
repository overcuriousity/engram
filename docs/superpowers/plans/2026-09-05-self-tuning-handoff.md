# Self-tuning: where things stand, for whoever comes next

Written 2026-09-05, at the end of stage 2b; updated the same day at the end of
stages 3a and 3b. It says what exists, what was decided along the way that the
spec does not, and what a later stage would have to build on.

**Spec:** `docs/superpowers/specs/2026-09-04-self-tuning-design.md`, with its
Part 5 replaced by `docs/superpowers/specs/2026-09-05-self-tuning-stage-3-design.md`.
The three decisions in "The three decisions this rests on" are settled; the
second is restated in the stage 3 spec. **Both specs are built in full.**
Stage 3 was two plans: 3a (the three knobs) and 3b (the corpus journal, the
two rules, the graveyard listing, and `review_min` on the ladder). Nothing in
either spec is unplanned or unbuilt.

**Branch:** `feat/observations`, off `master`. Stages 1, 2, 2b, 3a and 3b are on it,
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
| The corpus journal: one row per subject for every merge, replacement, discard, burial, promotion and filed moment; stamped by every undo | `corpus_actions` in `schema.sql`; `src/store/actions.rs`; writers in `jobs/dedupe.rs`, `core/ingest.rs` (`supersede_with`, `deprecate_with`), `store/artifacts.rs` (`bury`), `jobs/promote.rs`, `jobs/judgement.rs`; stamps in `web/ui.rs`, `web/due.rs` | 3b |
| The two corpus rules, run as the corpus half of the idle pass after the anchor check | `src/jobs/retract.rs` (`rule_one`, `rule_two`), called from `jobs/tune.rs` `pass` | 3b |
| The graveyard keeps the vector; the Reaped section lists it with Restore | `graveyard.vec` / `embed_model`; `Store::graveyard_list`; `insights.html` | 3b |
| `review_min` on the ladder, read by `relate.rs` off `Core::ranking`; moved on two band records | `core/ranking.rs` (`REVIEW_MINS`), `store/pairs.rs` (`band_record`), `jobs/tune.rs` (`next_review_min`, `review_step`) | 3b |
| Disclosure: the evolve section lists what the base did to the corpus, undos told apart, and what the rules last did | `web/insights.rs` (`action_str`, `rules_str`), `_evolve.html`, meta `evolve.retract.last` | 3b |
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
- **The corpus half runs before the ranking half's gates** (params drift,
  under watch), after the anchor. A base under watch still answers for what
  it hid. The pass's `Pass` carries `undone` and `restored` as flat counts.
- **There is no `stale` kind.** Nothing hides an artifact as stale on its
  own; the "Hidden as stale" heading shows operator deprecations. Journal
  kinds are merge, supersede, discard, reap, promote, moment.
- **Only the base's own actions are journaled.** A person's "Discard both"
  writes no row, and rule 2 restores only what has an open row: an artifact
  a person hid is not the base's to restore.
- **The journal is the memory.** Dedupe checks `action_was_undone` before a
  replacement, merge or discard and hands a repeat to a person; reap checks
  before the judge call and skips. `DecidedBy` gained `Evidence`, and
  `merge::undo` takes who is undoing.
- **Rule 1's observation window is at-or-before the action's second**: the
  clock is seconds and an observation in the hiding's own second was still
  made of a live subject.
- **Rule 2 compares within one replay** (hidden hits included, `Door::Judge`,
  rerank off) rather than against the captured pool, and needs the hidden
  hit to be *strictly* more similar than the best live one: a twin at the
  same similarity is no loss. Its cursor over give-ups is meta
  `evolve.retract.gave_up_after`.
- **A `review_min` move is watched by `lived`** like any generation; the
  wrong signal at the next pass is the step back. The "above" band runs to
  1.0, and a rung at or above `auto_supersede` is never offered. Rescues
  (`reap::rescue_one`) are not journaled.
- **The anchor rule:** trustworthy unless disagreements are at least two and
  at least equal agreements. Suspension adopts nothing, reverts nothing, keeps
  recording, and is the first thing Insights says.
- **The pass runs at the retention unit's cadence** (`feedback.sweep_hours`,
  six hours by default, with the empty-run backoff), behind a quiet check of
  `evolve.idle_secs`. Not on its own ticker.
- **`--print-config` does not name the live generation.** Stage 1 dropped that
  task: the flag is per instance and generations are per tenant. Insights is
  the disclosure.

## What a later stage would have to build on

The stage 3 spec's "What does not move yet" is the list: `stale_after_days` /
`stale_max_hits`, `activation_above`, and reap's `min_age_days` each have a
wrong signal after 3b — restores, unpromotes, exhumes are in the journal —
and no short signal that costs nothing. Each joins the ladder when a short
signal is defined for it, and `next_review_min`'s shape (two band records,
one-decision noise, wrong before short) is what it has to fit.

Beyond the spec, and deliberately out of it: rescues are not journaled;
promotions and filed moments are journaled but never taken back by the base;
the `ADDITIVE` list in `src/store/mod.rs` is at fifteen entries and every
one is nullable or defaulted. `CanJudge` still gates exactly one route, the
tuning Apply button; the pair queue, the restore buttons and every undo are
plain `Tenant`.

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
5. `2026-09-05-self-tuning-stage-3b.md` — built, Parts B and C in one plan,
   with the admissions at its top. The stage 3 spec is complete. Baseline at
   the end of 3b: 2584 passing, 0 failing, 1 ignored.
