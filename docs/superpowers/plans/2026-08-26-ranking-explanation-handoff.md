# Handoff — ranking explanation

Date: 2026-08-26
Branch: `ranking-explanation`, cut from `drop-the-scope-block`
Spec: `docs/superpowers/specs/2026-08-26-ranking-explanation-design.md`
Plan: `docs/superpowers/plans/2026-08-26-ranking-explanation.md`

Read the spec first, then the plan. This document says only where the work
stopped and what is not obvious from the diff.

## State in one line

Tasks 1–8 of 12 are done and committed. **Task 9 is half-applied and
uncommitted, and the tree does not compile under `cargo test`.** Fix that
first — the steps are below and it is roughly ten minutes.

## What is committed

```
bc7caab feat: an explain flag that gates rendering and nothing else
d99a622 test: pin the reconstruction against real Qdrant, not against ourselves
6e88006 feat: reconstruct the recency and pinned terms without a second query
1b29d55 feat: the reranker and priming say which hits they moved
ba25f8b feat: every ranked hit carries its retrieved rank and cap verdict
c314338 feat: the per-source cap says what it displaced and what came back
5eccdd5 refactor: a search returns an outcome, not only a duration
ac701f0 feat: the types a ranking explanation is made of
```

At `bc7caab` the library and its tests build, and `cargo test --lib` gives
**1930 passed, 2 failed** — the two failures pre-date this work, see below.

## Start here: finish Task 9

`src/mcp/mod.rs` is modified and not committed. What is already applied:

- `format_search_results` takes a second parameter,
  `explanation: Option<&crate::core::explain::SearchExplanation>`, and
  prepends a `_Pool: …_` line when it is `Some`.
- `why_line(&HitExplanation) -> String` is written, and the per-hit branch
  renders it when both the summary and the hit's own explanation are present.
- `SearchParams` has `explain: Option<bool>`; the `search` tool now calls
  `core.search_with(&query, cap, Door::Mcp)` and passes
  `explain.then_some(&outcome.explanation)`.
- Two new tests: `the_meta_line_stays_silent_unless_an_explanation_was_asked_for`
  and `an_explained_result_names_the_stage_that_failed_to_redistribute`.

**What is not done:** the existing tests in that file call
`format_search_results` with one argument. Some were updated by a regex pass,
some were not. `cargo build --lib --tests` names each one; add `, None` to
every remaining call. `None` is the right value at all of them — those tests
predate the explanation and assert on the meta line, which is unchanged.

Then `cargo test --lib`, expect 1932 passed and the same 2 pre-existing
failures, and commit as
`feat: MCP says why a hit is where it is, when asked`.

## Then Tasks 10, 11, 12

They are written out step by step in the plan and nothing about them has
changed. In short: `?explain=1` on the API returns
`{"results": […], "explanation": {…}}` and the bare call still returns the
array; the rail renders a short sentence on the `rail-why` line that
`src/web/templates/_results.html` already has; and `ROADMAP.md:498` moves from
a bullet to a `Built:` paragraph.

## Four things the diff will not tell you

**1. Two tests were already red before any of this.**

```
web::ui::tests::opening_a_passage_appends_the_one_that_follows_it
web::ui::tests::the_run_control_asks_for_one_more_than_is_on_screen
```

Verified failing on `master`, on `drop-the-scope-block`, and at `ac701f0`
which only adds a new module. They belong to the "continues in" work
(`fc505e0`, `7ef0f0b`), not to this branch. Do not try to fix them here, and
do not read them as a regression.

**2. The Qdrant contract test has never been run.**

`the_reconstructed_recency_term_matches_what_qdrant_scored` in
`tests/integration_qdrant.rs` compiles and that is all that has been proved.
No Qdrant was reachable on this machine and the Docker daemon did not answer.
Run it:

```
docker compose up -d
cargo test --test integration_qdrant the_reconstructed_recency_term -- --ignored
```

This is the most important unverified thing on the branch. It pins that
`core::explain::scoring_terms` reproduces Qdrant's `exp_decay` semantics. If
it fails, the bug is in `scoring_terms` (`src/core/explain.rs`) and not in the
test — and until it passes, the recency and pinned figures the three doors
render are unproven.

**3. Two plan deviations, both deliberate.**

The plan said to read `recency_half_life_days` and `pinned_boost` off
`self.ranking`. They are not there — `RankingParams` carries only
`recency_weight` and `per_source_cap`. They now live on `Core`, set from
`cfg.vector` at construction, beside `weak_below`, which had the same shape
and the same origin already. See `6e88006`.

The plan had `associated()` hits getting their explanation in a second pass
over the result list. They get it where `via` is known instead, in the builder
in `src/core/search.rs` and in `src/core/ask/mod.rs`. Same outcome, one fewer
traversal. See `ba25f8b`.

**4. `clippy` is not installed on this machine.**

Every commit here ran `cargo fmt` and the test suite, and none ran clippy.
`ROADMAP.md:713` records that CI is the only gate for it. Run it before
merging if your environment has it.

## What this is all for

The corpus-concentration measurement. Once this is deployed, the `explain`
flag over MCP is what says whether `cap_per_corpus` is quietly failing —
specifically `CapEffect::Refilled` on hits, and a non-zero `refilled` on the
pool line, which reads as "*N* displaced, *M* still in the answer".

Read `refilled`, not `displaced == refilled`. The first review of this branch
found that comparison degenerate: `search_inner` hands `cap_per_corpus` the
candidate pool as its refill target, so `kept` can never be short of it and
every displaced hit is always taken back — the equality held on every search
ever made, including the healthy ones. The cap's whole effect on the pool is
an order, and what it removes is decided by the truncate to `limit` at the end
of the search. `refilled` is therefore counted there, over the answer: any
number above zero is a hit that reached the caller over its source's cap
because there was nothing to put in its place. Zero is the rule holding, and
`displaced` above it with `refilled` at zero is the rule working.

Two things to know before gathering it. Nothing is stored — that was an
explicit decision (spec §10), so the figure has to come from deliberate
searches rather than from history. And every such search writes: `Door::Mcp`
is `captured()` (`src/store/feedback.rs:52`) and the `search` tool sets
`mark: true`, so each probe bumps activation, writes a `search_event` and
stamps `last_seen_at`. Keep the probes few, and name them to the operator
before running them.

## Open question for the operator

OAuth 2.1 for `/mcp` (`ROADMAP.md:703`) was agreed as the next branch after
this one, with its own spec. It has not been started, and nothing about it has
been designed yet.
