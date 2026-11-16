# A gate that does not refuse, notes that remember what found them, and a deeper look after a miss

Date: 2026-11-16. One design, three parts, one pull request. Owner's decisions are
marked as such; everything else is the designer's call and is listed at the end.

File and line references come from a read of `e7ee994`; verify before relying.

## Intent

The owner asked Decide whether a pasted situation fell under a rule that is in the
base, and got "Nothing here: the base does not hold this". The note was there.
Decide never weighed it: either search did not return it among the eight passages
read, or the per-passage check (`bears`) scored it under 0.5 and nothing else
cleared the line either.

Two things are wrong, and neither is about that one question.

1. **Decide refuses on a fixed line nothing tunes.** `decide.bears_min` is the one
   Decide setting the base never moves (`src/jobs/tune.rs:882`), and a miss of it is
   reported as a fact about the base.
2. **The base cannot learn that a query leads to a note.** What it learns from use
   is global: a handful of ranking settings. The decision model labels the
   candidates of every recorded search, but only those search already returned, so
   it can say the order was wrong and never that the right note was missing. A note
   is found by its own wording and by nothing else.

The memory should get better at finding what a person means, not only what their
words resemble. This design makes three changes toward that:

- **Part 1.** Decide's gate stops refusing and tunes its line from verdicts.
- **Part A, trails.** A note keeps the queries it was confirmed to answer, and
  search can find it through them.
- **Part B, the deep look.** When the base is quiet, the decision model looks
  further down for searches and questions that found nothing, and what it finds
  becomes a trail.

engram does not know what kind of text it holds. Nothing here may assume a kind of
note, a kind of question, or a relation between them (owner's decision).

## Goals

- A Decide run over passages that exist always returns a verdict, and says when it
  rests on loose matches.
- The line between "bears" and "loose" moves on the person's verdicts.
- A caller's `situation` takes part in retrieval.
- A confirmed pairing of a query and a note makes that note findable by similar
  queries, at every door, without a reindex and on all three vector stores.
- A search or question that found nothing is looked at again, further down, without
  a person waiting.
- The person's verdicts stay the authority: what only the model said is used only
  while the model's labels agree with the person's, by the rule the sweep already
  uses (`Core::labels_count`).
- Evaluation stays honest: a replay never finds a note through the trail made from
  the very query being replayed.

## Non-goals

- A second field for the situation in the web form (owner's decision: the box is
  the question; context comes from retrieval).
- Splitting the box text into parts, by rule or by model.
- A new wording for `bears`. See "Approaches".
- Writing anticipated queries for a note with a chat model, a live judging stage in
  the search path, and ranking by kind of need. These are later work, for the issue
  tracker.
- Trails on the Android app's own surfaces beyond what the API already carries.
- Changing how model labels earn their trust (`label_overlap_min`,
  `label_agree_min`, `label_select_max`).

## Anti-goals

- A verdict presented as settled when no note cleared the line.
- A trail that confirms itself: made from a model's guess, then counted as evidence
  because search now returns the note.
- A trail that outlives a person's contradiction of it.
- A measurable slowdown of search. The trail step is arithmetic over vectors
  already in memory; it calls no model and no service.
- Query text kept after the operator has deleted captured searches.

## What this supersedes

Spec 1 (`2026-11-13-decide-1-verb-design.md`) has the anti-goal "A verdict from the
model's own prior. If no passage bears on the question there is no verdict", and
"Done when" pins a scripted decider at 0.99 that still returns `not_held`. The
owner has decided otherwise (2026-11-16): the gate tunes itself or goes. Part 1
replaces step 4 of that spec. The prior marker on every bar still shows what the
model would have said without notes.

Spec 2 (`2026-11-13-decide-2-judgement-design.md:464-484`) says `bears_min` is not
moved. Part 1 moves it.

---

## Part 1: Decide

### How a decision is made, as changed

Steps as in spec 1; only these differ.

**1. Retrieve.** Without a `situation`: unchanged, `search_with` on `q`
(`src/core/decide/mod.rs:536`). With one: a second search on the situation, same
filters, same door, same limit. The situation is cut to what the embedder takes
(`search_inner` refuses a query over 0.8 of `max_input_tokens`,
`src/core/search.rs:1172`); if it still cannot be searched, the decision goes on
with `q` alone. The two lists are interleaved by rank, `q`'s first, one row per
artifact, retired rows dropped, cut to `limit`. The vector recorded for the decision
stays `q`'s.

**3. Single reads.** Unchanged, `bears` included, same wording, same version.

**4. Evidence.** The line is `bears_at`: the tuned line for this model's era, else
`decide.bears_min` (as `Core::settled_at` reads its line,
`src/core/decide/mod.rs:263`).

- Passages with `bears >= bears_at`: as today. The decision is not weak.
- None, and at least one passage was read: the `decide.weak_take` (3) passages with
  the highest `bears`, ties by rank, are the evidence. The decision is **weak**.
- No passage at all: `not_held`, as today. This is the only case left.

**5. Joint read.** Unchanged, over that evidence.

**6. Standing.** As today, then: a weak decision that would be `settled` is
`leaning`. `split` stays `split`.

### What a weak decision is, everywhere

A decision gains `weak: bool`. A weak decision is held: it has answers and evidence.

| Where | Change |
|---|---|
| `Decision`, `PassageRead` | `Decision.weak`. A read under the line keeps `bearing: false`. Evidence rows of a weak decision have `joint: true` and `n` set, `bearing: false`. |
| JSON (`src/core/decide/wire.rs`) | `"weak": true`, written only when true. "Absent means false", as a search hit's `weak` (`docs/api.md:53`); the word means the same thing on both: a loose match. |
| Store | `decide_events.weak INTEGER NOT NULL DEFAULT 0`, an `ADDITIVE` entry. `decide_evidence.n` is set for every row in the joint read. |
| Observations | A weak decision writes no `Cited` observation and marks nothing cited (`src/store/decisions.rs:125`, `src/core/decide/mod.rs:496`). Notes under the line are not positive evidence. |
| Gaps (`decide_gaps_from!`, `src/store/gaps.rs:138`) | A weak decision is an open gap unless judged `right`: `(not_held = 1 AND verdict <> 'wrong') OR (weak = 1 AND verdict IS NOT 'right') OR (not_held = 0 AND verdict = 'nothing_here')`. |
| Verdicts (`src/core/decide/verdict.rs`) | `sides` reads the evidence in the joint read, not the bearing rows, so Right on a weak decision stamps the notes that agree, as on any other. All three buttons show. |
| Web pane (`_decision.html`) | Bars as for any decision. The standing line reads `Leaning, from loose matches.` or `Split, from loose matches.` |
| Web rail | Head: `Weighed the closest 3 of 8`. The rest line: `5 more were not weighed`. While streaming, a row under the line still says "does not bear"; `done` replaces the rail as it does today. |
| `progressText` (`assets/js/decide.js:51`) | With no bearing read after the last one: `Weighing the closest together`. |
| CLI (`src/cli/decide.rs`) | Same standing line and rail wording. Exit `0`: there are answers. `1` stays "no answers". |
| MCP (`format_decision`) | Same standing line; the count line says "were not weighed". |
| Insights | See "Tuning". |
| `docs/api.md`, README | The decide row and the standing sentence. |

"Loose" is the page's existing word for a hit under `weak_below`; a weak decision
uses it and adds no new term.

### Tuning the line

The line is a threshold of its own: `question_id = "decide_bears"`, era
`settled_era(model)`, kept in `decision_thresholds` beside `decide_settled`
(`src/jobs/tune.rs:881-904` is the pattern; `move_thresholds`, `thresholds_in_force`).

Evidence, over decisions under the configured decider and `BEARS_VERSION` made since
the line last moved, first question only:

| Decision | Verdict | Says |
|---|---|---|
| not weak | `nothing_here` | the line let through notes that bore nothing: **too low** |
| weak | `right` | notes under the line answered it: **too high** |
| not weak | `right` | the line was fine |
| weak | `nothing_here` | the line was fine |

`wrong` says nothing about the line: a wrong answer may rest on relevant notes.

Let `n` be the four cells together. Nothing moves under `decide.tune_min` (30) of
them. Then, with `decide.undo_target` (0.05) and `STEP` (0.02):

- too low `/ n > undo_target` and too low `>` too high: the line rises a step.
- too high `/ n > undo_target` and too high `>` too low: the line falls a step.
- otherwise it stays.

Bounds `BEARS_BOUNDS = (0.20, 0.80)`. `next_act_at` is not reused: it can only come
down after thirty acts above the line, and a line that is too high produces few of
those. The move is recorded with a reason in the house form, e.g. "4 of 31 weak were
judged right".

Insights (`judgement_lines`, `src/web/insights.rs:948`): a line
`decide_bears · bears 0.50 · as configured` beside `decide_settled`, and a share
"weak and right" beside the three there. The field doc at `:943` and the test
`split_at_and_bears_min_are_never_moved` (`src/core/decide/mod.rs:1684`) change to
say only `split_at` stays.

The gate needs no evidence to stop refusing. Tuning decides only how often a verdict
is marked as resting on loose matches.

### Config

```toml
[decide]
# The closest passages weighed when none reaches the line (1 to 8).
# weak_take = 3
```

`bears_min` keeps its name and becomes the starting line.

---

## Part A: Trails

A **trail** is one query that a note was confirmed to answer, kept on the note with
the query's vector. The name is new on purpose: `artifacts.cues` already means the
questions a model-written note was written for, and those stay what they are.

### Why not in the vector store

One point per artifact, and a Qdrant collection cannot gain a named vector without a
rebuild (`src/vector/qdrant.rs:24`). The existing per-note vector set, `ctx`, is 45
wide and holds page-view situations. A second named vector would force `--reindex`
on every live base, would be dropped by every re-embed (`upsert` sends the whole
point), and would need three backend implementations and three test wrappers.

Trails live in the tenant's SQLite store and are matched in the process. A base is
one person's (`Tenants::alias`, `src/tenants.rs:142`), trails are few compared with
notes, and both sides of the comparison are query-side embeddings the base already
has.

### Storage

```sql
CREATE TABLE IF NOT EXISTS trails (
  id TEXT PRIMARY KEY,
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  query TEXT NOT NULL,
  query_vec BLOB NOT NULL,
  vec_dim INTEGER NOT NULL,
  embed_model TEXT NOT NULL,
  -- 'verdict' | 'ask' | 'decide' | 'label' | 'deep'
  source TEXT NOT NULL,
  -- The event the pairing came from.
  source_id TEXT NOT NULL,
  -- 'person' | 'model'
  tier TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  -- Times a search it lifted a note in was then confirmed, and contradicted.
  matched INTEGER NOT NULL DEFAULT 0,
  missed INTEGER NOT NULL DEFAULT 0,
  retired_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_trails_live
  ON trails(artifact_id, query) WHERE retired_at IS NULL;
```

`search_candidates` gains `trail_id TEXT` (nullable, `ADDITIVE`): the trail that
lifted or brought this candidate, if one did.

### Where a trail comes from

| Source | When | Query and vector | Note | Tier |
|---|---|---|---|---|
| `verdict` | a search is judged a hit (`judge_hit`, `src/store/feedback.rs:511`) | the search's | `expect_id` | person |
| `ask` | an ask is judged Right (`confirm_cited`, `src/core/ingest.rs:1216`) | the ask's | each cited note the answer used | person |
| `decide` | a decision is judged, and the right answer is known | the decision's `q` | each note that agrees with the right answer | person |
| `label` | the labeller finds a relevant candidate in an unjudged search (`model_expected`, `src/store/search_labels.rs:319`) | the search's | that candidate | model |
| `deep` | Part B finds a note | the source's | the found note | model |

Not from an open, and not from a citation in an unjudged answer: both are evidence
the sweep counts, neither is a confirmation.

Rules:

- A source row with no vector under the live embed model makes no trail.
- A trail whose query is within cosine 0.95 of a live trail on the same note
  replaces nothing: the older stays, and takes the higher tier of the two.
- At most `trails.per_note` (8) live trails a note; the one that goes is the model
  tier before the person tier, then the oldest.
- At most `trails.max` (20000) live trails a base, by the same order.
- Undoing the verdict that made a person trail retires it (`unjudge`,
  `unjudge_decision`, an ask's undo).
- A note deleted takes its trails with it (the foreign key). A trail on a note that
  was superseded counts for what superseded it, followed forward as
  `eval::satisfied_by` follows a chain (`src/eval/mod.rs:12`), at most 8 hops.
- `[learn] mode = off`: none are made and none are used.
- The operator's "delete captured searches, questions and situations"
  (`forget_situations`, `src/web/settings.rs:198`, `src/web/client.rs:722`) deletes
  every trail: a trail holds query text.
- A trail under another embed model is not used. When the base is quiet its query is
  embedded again under the live model and the row is replaced, a bounded number a
  pass, as `sleep::rehearse` remints a cue probe (`src/jobs/sleep.rs:286-343`).

### The trail step in search

In `search_inner` (`src/core/search.rs:1150`), at every door. Skipped when
`trail_weight` is 0, when the query has no vector (the keyword fallback), or when
there are no live trails.

1. **Match.** Every live trail under the live embed model whose cosine with the
   query vector is at least `match_at`. `match_at` is `trails.match_at` (0.80),
   never under the measured line at which two unrelated queries sit, read as
   `weak_below` reads its line (`gaps::line_above`, `src/core/mod.rs:275`).
   Model-tier trails count only while `Core::labels_count()` holds; that answer is
   cached and refreshed by the `Decisions` tick, never computed in a search.
2. **Leave-one-out.** At `Door::Judge`, a trail within cosine 0.999 of the query
   vector is left out. A replay is the stored query of some past event; the trail
   made from that same query must not find its note. This covers the sweep
   (`rank_of`), the probes (`rehearsed_under`) and both retraction rules, because
   all of them replay through `Door::Judge` (`src/eval/sweep.rs:224-297`).
3. **Resolve.** The best trail per note, followed forward through supersession.
   Notes not already in the pool are loaded (`payloads_of`) and put through the
   search's filter in the process: tags, category, status.
4. **Join the pool.** After `cap_per_corpus` (`:1330`), so a trail note neither
   displaces nor is displaced by the cap. A joined note has `similarity: None`: no
   opinion, so it is not called loose and the cliff passes over it, as for a
   lexical hit.
5. **Reorder.** After the reranker (`:1472`) and before priming (`:1504`), over
   positions from 0: each row's score is `1 / (pos + 2)`, plus
   `trail_weight / (tpos + 2)` where `tpos` is its place among the matched notes by
   cosine. Rows are sorted by that; ties keep their order. At the shipped weight the
   best-matched note lands near third place and cannot pass a first row that has no
   trail of its own. After the reranker, because a reranker scores query against
   text and would sink exactly the notes a trail exists to surface.
6. **Explain and record.** `HitExplanation` (`src/core/explain.rs`) gains
   `trail: Option<{ query, similarity }>`, shown in the rail's "why" as
   `found before for «…»`. A recorded candidate carries `trail_id`.

Cost: one pass over the live trails per search. At the cap, 20000 vectors of 768
dimensions, about 60 MB held and a few milliseconds. `Core` holds the live trails in
memory, loaded on first use and replaced when one is made or retired.

### `trail_weight` is a ranking setting

It joins `RankingParams` and `GenerationParams` with a ladder
`TRAIL_WEIGHTS = [0.0, 0.5, 1.0]`, shipped at 0.5 (`[trails] weight`). So the idle
pass decides how far to trust trails by the same replay it uses for every other
setting, the probe yardstick can refuse a move, and the lived watch reverts one that
serves worse. `apply_learn_mode` zeroes it for `off` and `learning`, as it does
`prime_lift`. A stored generation without the field reads the config value.

With leave-one-out, a replayed pair gains from trails only when a *different* query's
trail finds its note. That is the claim trails make, and the only one the sweep can
score.

### A trail answers for itself

When a recorded search's candidate carries a `trail_id`:

- judged a hit on that note, or labelled relevant (`yes >= relevant_at`): the
  trail's `matched` rises.
- labelled with `yes` under `answers_query`'s `ask_at` (0.60): `missed` rises.

A trail with `missed >= 3` and `missed > matched` is retired. The labeller judges a
note against the new query alone and does not know how the note got there, so a
trail cannot confirm itself.

### Config

```toml
# What a note was found for. Used at [learn] mode = full.
# [trails]
# How much a matching trail counts in the order (0 turns trails off). On a ladder.
# weight = 0.5
# Similarity between a query and a trail's from which the trail matches.
# match_at = 0.80
# Live trails a note keeps, and a base keeps.
# per_note = 8
# max = 20000
```

Insights, one line under retrieval: `trails: 41 on 30 notes · 12 by you ·
7 searches found a note through one`.

---

## Part B: The deep look

`Stage::Deepen`: one unit per source, run only while the base is quiet. It asks the
labeller's question, `answers_query` (`src/core/judging/catalogue.rs:210`), at the
labeller's line (`label::relevant_at`), of notes search did not show.

### What gets a deep look

In this order:

1. **Open gaps** of every kind (`open_gaps`, `src/store/gaps.rs:302`): an ask or a
   search judged "nothing here", a search that matched nothing, an unsatisfied
   pursuit, an uncovered ask subject, and a Decide gap, which after Part 1 includes
   every weak decision not judged right. Each already has its text and its vector
   under the live embed model.
2. **Recorded searches the labeller left out**: labelled under the era in force,
   no candidate relevant, not judged a hit, vector under the live embed model.
   Searches with a `gave_up` observation first.

A source gets one deep look per era (question version and decision model), kept in:

```sql
CREATE TABLE IF NOT EXISTS deep_looks (
  -- A gap kind ('ask' | 'search' | 'unmatched' | 'pursuit' | 'subject' | 'decide') or 'labelled'.
  source_kind TEXT NOT NULL,
  source_id TEXT NOT NULL,
  -- '' marks the look as done, as the labeller's step-aside row does.
  artifact_id TEXT NOT NULL,
  yes REAL NOT NULL,
  question_version INTEGER NOT NULL,
  model TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY (source_kind, source_id, artifact_id)
);
```

Rows of a source that is gone are trimmed where gap coverage is trimmed
(`trim_gap_coverage`, `src/store/gaps.rs:779`).

### The pool

1. The source's stored vector is replayed (`remember_query_vector`, then
   `search_with_ranking` at `Door::Judge`, `mark: false`, no reranker, the running
   parameters, `limit` 50).
2. The notes linked to the first five (`artifact_links`, as `associated` reads
   them, `src/core/search.rs:832`) are appended.
3. Dropped: notes already asked about for this source in this era (its
   `search_labels` rows, its `gap_rejections`, its `deep_looks`), notes not in
   results, and notes that do not fit one request whole (`state::one`; a cut note is
   not judged, as in the labeller).
4. The first `decide.deep_max` (24) remain, in that order.

Each is asked alone, on the `background_light` lane. The look ends at the first
note at or above the line, or when the pool is spent. Every answer is stored. A
failed call is handled as the labeller handles one (`label::run`,
`src/jobs/label.rs:61-105`): no partial result, the unit retried, a refusal counted.

### What a find does

- **A trail** on the found note, source `deep`, tier model.
- **A gap source:** the gap is covered by the note, `CoveredBy::Model` (`cover_gap`,
  `src/store/gaps.rs:637`). That is what `cover_answering` already does for a new
  capture; the deep look does it for notes that were there all along.
- **A labelled search:** while `labels_count()` holds, it is a pair for the sweep:
  expected, the found note; served rank, none. `evidence_pairs`
  (`src/eval/sweep.rs:356`) takes these with the model's other pairs, inside the
  same room.

Nothing found: the done row, and for a gap its answers are also written to
`gap_rejections`, so `cover_answering` does not ask the same pair again.

A person's verdict is never changed. A search judged "nothing here" stays judged so;
its gap shows as covered by the model, as any gap covered that way does today.

### Pacing

Armed from the `Decisions` tick after labels (`src/jobs/decisions.rs:145-150`), with
a quarter of what the tick's budget has left, and only while `scan`'s quiet test
holds (`:245-270`). A look costs its pool size, and a queued look is counted by
`judging_queued` (`src/core/judging/mod.rs:211`). At the defaults that is about one
deep look every fifteen minutes of quiet; `decide.max_per_tick` raises it.

Insights, one line: `deep looks: 18 · 5 found a note search had missed`. That second
number is the first measure the base has of what retrieval misses.

### What it cannot find

A note the replay does not bring into the first fifty and that nothing among the
first five links to. The deep look widens the net; it does not read the base.

### Config

```toml
[decide]
# Notes a deep look asks about, at most (1 to 50).
# deep_max = 24
```

---

## How the three meet

A question finds nothing clear. Decide answers from the closest notes and says so;
the decision is an open gap. In the next quiet quarter hour the deep look asks about
twenty-four notes further down and finds one that answers. The gap closes, and the
note keeps the question as a trail. A later query that reads like that question
finds the note in third place, with "found before for «…»" beside it. If the person
confirms it there, the note has a trail of theirs; if the labeller keeps saying it
does not help, the trail is retired.

The model-tier half of that waits on `labels_count()`: thirty searches the person
judged, with the model agreeing. Until then the base records what the model found
and shows it on Insights, and only what the person confirmed changes what is served.

## Constraints

- AGENTS.md and the dev manifest v2026.11.1, profile everyday: tokens only, no
  inline styles or scripts, symbol and colour for status, layout from 390 px,
  `oc-check .`, the review checklist.
- Queries are compile-time checked; a runtime query carries a `// runtime:` reason
  (`every_runtime_query_says_why`). `.sqlx/` is regenerated and committed.
- Schema changes go through `Store::migrate`: new tables in `schema.sql`, new
  columns also in `ADDITIVE`.
- A new `Stage` touches `src/store/jobs.rs` (`Stage::ALL` and its count, `as_str`,
  `parse`, `class`: background, `calls_a_generator`: no, `reads_a_capture`: no, and
  the tests that pin the count), the dispatcher in `src/jobs/mod.rs`, the operator
  reprocess match (`src/core/ingest.rs:1636`), `sweep_label` (`src/web/ui.rs:148`)
  and `judging_queued`.
- `docs/api.md` is what clients rely on: fields are added, none renamed or removed.
- No probability shown as the truth, no percent sign, no "likely" (spec 1).
- Android runs no decision model and parses with unknown keys ignored; nothing
  there changes.

## Approaches

**The gate.** (a) Soft, with a line that tunes itself — chosen. (b) Dropped: every
retrieved passage weighed; unrelated notes then pull on the answer and on `split`,
and nothing marks a verdict that rests on poor material. (c) Hard, self-tuned: the
wrong refusals continue until thirty verdicts exist.

**The web box.** (a) The box stays the question, whole — chosen by the owner.
(b) A situation field — rejected by the owner. (c) Splitting the box by rule — a
guess that changes the question silently when wrong.

**The wording of `bears`.** Using the labeller's `answers_query` for Decide's check
was considered and dropped. The two are read over different states (Decide's has the
situation, the capture day, and cuts a long note; the labeller's has none of these
and refuses one), and they serve different lines (0.90 for "relevant", about 0.50
for "weigh it"), so a shared wording would share no data. It would bump
`BEARS_VERSION` and discard the tuned `settled_at` and every recorded decision's
standing as evidence.

**Where trails live.** (a) SQLite, matched in the process — chosen. (b) A named
multivector in the collection: a forced `--reindex`, lost on re-embed, three
backends. (c) A second collection of trail points: a second thing to keep in step
with lifecycle, and no answer for the contained build.

**How a trail changes the order.** (a) A weighted rank term after the reranker, on a
ladder — chosen: the base decides how much trails are worth. (b) A bounded lift like
priming: a note outside the list cannot be lifted into it. (c) Always first: one
wrong trail owns a query.

**What the deep look reads.** (a) A deeper replay plus links, capped — chosen.
(b) Every note in the base: unbounded model time. (c) Only new captures: what
`cover_answering` does today, and blind to notes already there.

## Done when

- With a scripted decider whose `bears` is 0.02 for every passage, a decision over a
  base of three notes has answers, `weak: true`, three evidence rows with
  `joint: true`, a standing that is not `settled`, no `Cited` observation, and is an
  open Decide gap. Judged Right, it is no gap.
- A decision over an empty base is `not_held`, as today.
- Thirty-one judged decisions of which four are weak and right move `decide_bears`
  down one step with a stated reason; the mirror case moves it up; `wrong` verdicts
  move nothing.
- A decision with a `situation` reads a passage that only the situation's search
  returns.
- A search judged a hit on a note makes a person trail. A later search whose vector
  is within `match_at` of it, and for which the store does not return the note,
  shows the note with `trail` in its explanation. With `trail_weight = 0` it does
  not.
- The same search replayed at `Door::Judge` with the trail's own vector does not
  find the note through it.
- A model-tier trail changes no result while `labels_count()` is false.
- Undoing the verdict retires the trail. Deleting captured searches deletes all.
- A trail whose candidate is labelled under the ask line three times is retired.
- A gap whose answer is at rank 30 of its replay is covered by the model after one
  deep look, and the note has a `deep` trail. A second tick asks nothing more of it.
- A deep look does not run while a search happened inside `evolve.idle_secs`.
- Every command in AGENTS.md passes, on both feature sets.

## Not measured here

No decision model was reachable while this was designed. Three numbers are the
designer's estimates and want a run on the owner's endpoint: `weak_take` (3),
`match_at` (0.80), and `deep_max` (24). Whether `bears` scores a short note low
against a long question, which is the suspected cause of the report that started
this, is also untested; Part 1 makes it harmless, not known.

## Calls made by the designer

- The names: `weak`, "from loose matches", trail, deep look, `Stage::Deepen`,
  `decide_bears`.
- A weak decision exits the CLI with `0`.
- The tuning rule for the line, its bounds and that `wrong` is silent.
- Model-tier trails and deep-look pairs wait on `labels_count()`. The alternative is
  to use them from the first day at a lower weight; that trades the owner's rule
  that a label never stands in for a verdict against a base that improves before
  thirty searches were judged.
- Opens and unjudged citations make no trail.
- The reorder formula, the ladder and the shipped weight of 0.5.
- The three thresholds for a trail's retirement (3 misses, more misses than matches)
  and the near-duplicate line (0.95).
- The deep look ends at its first find.

## Left to the builder

- The interleave when one of the two searches returns fewer rows.
- How `Core` holds and refreshes the live trails and the cached `labels_count`.
- Where re-embedding of stale trails runs, and how many a pass.
- The order of work across the pull request, and its commits.
- The exact Insights wording, within the manifest's rules for copy.
