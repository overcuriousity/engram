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
- Autonomy from the first day (owner's decision, 2026-11-16): the base improves on
  what the model finds and on what use leaves behind, over months, with nothing a
  person has to judge, review or maintain. A verdict, where one is given, outranks
  the model on the same question; none is ever required.
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

## Anti-goals

- A verdict presented as settled when no note cleared the line.
- A trail that confirms itself: made from a model's guess, then counted as evidence
  because search now returns the note.
- Anything that works only once a person has pressed enough buttons.
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

## Trusted until shown otherwise

`Core::labels_count` (`src/jobs/label.rs:178`) lets the model's labels steer a sweep
only after thirty searches the person judged, with the model agreeing. A person who
judges nothing never gets there, and the labels count for nothing for good. That is
the opposite of the goal above, so the rule turns round, for the labels it already
governs and for everything new here:

- The model's labels count unless they are shown to be bad.
- **Shown by the person:** over at least `label_overlap_min` judged searches, the
  hit rate is under `label_agree_min` or the selectivity over `label_select_max`.
  Unchanged figures; they now stop the labels and no longer have to start them.
- **Shown without anyone:** the selectivity over the last 200 labelled searches,
  judged or not, is over `label_select_max`. A model that calls everything relevant
  is caught with no verdict given.

Insights keeps both figures and says which, if either, has stopped the labels.

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
| Gaps (`decide_gaps_from!`, `src/store/gaps.rs:138`) | A weak decision is an open gap unless judged `right`. The first arm keeps its `COALESCE`: `(not_held = 1 AND COALESCE(verdict, '') <> 'wrong') OR (weak = 1 AND COALESCE(verdict, '') <> 'right') OR (not_held = 0 AND verdict = 'nothing_here')`. |
| Verdicts (`src/core/decide/verdict.rs`) | All three buttons show. A verdict on a weak decision is stored, moves the gap by the rule above and counts toward the line, and does nothing else: no note is stamped confirmed, no pair is queued, no trail is made. The person confirmed the answer, not which of the loose notes gave it, and a loose note's own read lands on the right side by chance half the time. Which note answered is the deep look's to find (Part B). |
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

A decision nobody judged is read a second time when the base is quiet (Part B):
its evidence is asked the labeller's question, alone, note by note.

| Decision, unjudged | Second reading | Says |
|---|---|---|
| not weak | no evidence note reaches `answers_query`'s `ask_at` (0.60) | **too low** |
| weak | an evidence note is relevant (`>= relevant_at`) | **too high** |
| either | anything else | the line was fine |

A verdict on a decision replaces its second reading. While the labels are stopped
(above), second readings count for nothing.

Let `n` be the cells of both tables together. Nothing moves under `decide.tune_min` (30) of
them. Then, with `decide.undo_target` (0.05) and `STEP` (0.02):

- too low `/ n > undo_target` and too low `>` too high: the line rises a step.
- too high `/ n > undo_target` and too high `>` too low: the line falls a step.
- otherwise it stays.

Bounds `BEARS_BOUNDS = (0.20, 0.80)`. `next_act_at` is not reused: it can only come
down after thirty acts above the line, and a line that is too high produces few of
those. The move is recorded with a reason in the house form, e.g. "4 of 31 weak held a
note that answered".

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
  created_at INTEGER NOT NULL,
  -- Times a search it lifted a note in was then confirmed, and contradicted.
  matched INTEGER NOT NULL DEFAULT 0,
  missed INTEGER NOT NULL DEFAULT 0,
  -- The last confirmation, else `created_at`; what ageing reads.
  used_at INTEGER NOT NULL,
  retired_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_trails_live
  ON trails(artifact_id, query) WHERE retired_at IS NULL;

-- Every pairing that stands behind a trail. A trail with none is retired.
CREATE TABLE IF NOT EXISTS trail_sources (
  trail_id TEXT NOT NULL REFERENCES trails(id) ON DELETE CASCADE,
  -- 'verdict' | 'ask' | 'decide' | 'label' | 'deep'
  source TEXT NOT NULL,
  -- The event the pairing came from.
  source_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY (trail_id, source, source_id)
);
```

A trail's **tier** is read off its sources: `person` when one of them is `verdict`,
`ask` or `decide`, else `model`.

`search_candidates` gains `trail_id TEXT` (nullable, `ADDITIVE`): the trail that
lifted or brought this candidate, if one did.

### Where a trail comes from

| Source | When | Query and vector | Note | Tier |
|---|---|---|---|---|
| `verdict` | a search is judged a hit (`judge_hit`, `src/store/feedback.rs:511`) | the search's | `expect_id` | person |
| `ask` | an ask is judged Right (`confirm_cited`, `src/core/ingest.rs:1216`) | the ask's | each cited note the answer used | person |
| `decide` | a decision that is not weak is judged, and the right answer is known | the decision's `q` | each bearing note that agrees with the right answer | person |
| `label` | the labeller finds a relevant candidate in an unjudged search (`model_expected`, `src/store/search_labels.rs:319`) | the search's | that candidate | model |
| `deep` | Part B finds a note | the source's | the found note | model |

Not from an open, and not from a citation in an unjudged answer: both are evidence
the sweep counts, neither is a confirmation.

Rules:

- A source row with no vector under the live embed model makes no trail.
- A pairing whose query is within cosine 0.95 of a live trail on the same note
  makes no second trail: it is added to that trail's sources.
- At most `trails.per_note` (8) live trails a note; the one that goes is the model
  tier before the person tier, then the oldest.
- At most `trails.max` (20000) live trails a base, by the same order.
- Undoing a verdict removes its source row (`unjudge`, `unjudge_decision`, an ask's
  undo). A trail left with only model sources is a model trail again; one left with
  none is retired.
- Ageing follows the store's rule that every table holding typed words ages
  (`expire_feedback`, `src/store/feedback.rs:858`): a model trail is deleted
  `feedback.retain_days` after `used_at`. A person trail stays, as the judged row it
  came from stays.
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
   Model trails match from the first day, at half a person trail's weight (step 5),
   and not at all while the labels are stopped; that answer is cached and refreshed
   by the `Decisions` tick, never computed in a search.
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
   lexical hit. Its `score` is the lowest in the pool.
   The reranker is given the whole pool, joined notes included, and its cut to
   `top_n` (`:1451-1466`) does not apply to a matched note: the matched notes it cut
   are put back after the rows it kept, in the order it gave them, with the score it
   gave them. Without this the reranker would drop exactly the notes step 5 exists
   to lift, at `Door::Ask` most of all.
5. **Reorder.** After the reranker (`:1472`) and before priming (`:1504`), over
   positions from 0: each row's score is `1 / (pos + 2)`, plus
   `tier * trail_weight / (tpos + 2)` where `tpos` is its place among the matched
   notes by cosine and `tier` is 1 for a person trail, 0.5 for a model trail. Rows are sorted by that; ties keep their order. At the shipped weight the
   best-matched note lands near third place by a person trail and near seventh by a
   model trail, and cannot pass a first row that has no trail of its own. After the reranker, because a reranker scores query against
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

When a shown candidate of a recorded search carries a `trail_id`:

| What happens to the search | Person trail | Model trail |
|---|---|---|
| that note is opened | `matched` rises, `used_at` is now | the same |
| judged a hit on that note | the same, and the trail gains a `verdict` source | the same: it is a person trail from here |
| judged "nothing here" | `missed` rises | `missed` rises |
| that candidate labelled under `answers_query`'s `ask_at` (0.60) | nothing | `missed` rises |
| that candidate labelled relevant | nothing | nothing |

A trail with `missed >= 3` and `missed > matched` is retired.

Use confirms a trail and the model may withdraw one; the model never confirms one.
The labeller and the deep look are the same model asking the same question, so the
model agreeing with a note its own trail brought is no second opinion. For the same
reason a candidate that carries a model trail's id makes no `label` trail and is not
the expected note of a model pair in `evidence_pairs`. The model never touches a
trail a person stands behind.

So a model trail lives on what happens after it: it is opened and stays, it is
labelled unhelpful or ignored and goes, by retirement or by ageing. Nobody has to
look at it.

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
2. **Weak decisions judged right.** Their gap is closed, but which note answered is
   not known. The pool starts with the notes that were weighed.
3. **Decisions nobody judged**, for the second reading that tunes Decide's line
   (Part 1). The look reads the decision's evidence and nothing further: there is no
   pool, no trail and no coverage. An unjudged weak decision is an open gap and gets
   the whole look under 1; its evidence is read first and counts as its second
   reading.
4. **Recorded searches the labeller left out**: labelled under the era in force,
   no candidate relevant, not judged a hit, vector under the live embed model.
   Searches with a `gave_up` observation first.

A source gets one deep look per era (question version and decision model), kept in:

```sql
CREATE TABLE IF NOT EXISTS deep_looks (
  -- A gap kind ('ask' | 'search' | 'unmatched' | 'pursuit' | 'subject' | 'decide'),
  -- 'weak_right', 'second_reading' or 'labelled'.
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

`deep_look_failures(source_kind, source_id, question_version, model, refused,
retry_at)`, keyed by the first two, is `search_label_failures` for a deep look
(`src/store/schema.sql:457`).

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
note at or above the line, or when the pool is spent, and its answers are then
stored together. A failed call is handled as the labeller handles one (`label::run`,
`src/jobs/label.rs:61-105`): nothing is stored, the unit is retried, a refusal is
counted in `deep_look_failures`, and from `LABEL_GIVE_UP` of them the source is
given up for the era.

### What a find does

- **A trail** on the found note, source `deep`, tier model.
- **A weak decision judged right:** the trail, and nothing else.
- **A gap source:** the gap is covered by the note, `CoveredBy::Model` (`cover_gap`,
  `src/store/gaps.rs:637`). That is what `cover_answering` already does for a new
  capture; the deep look does it for notes that were there all along.
- **A labelled search:** unless the labels are stopped, it is a pair for the sweep:
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

None of that waits for a person. If the person confirms the note there, the trail
is theirs and counts double; if they never press anything, the trail still stands
or falls by whether it is opened and what the labeller says of it.

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
  open Decide gap. Judged Right, it is no gap, no note is stamped and no trail is
  made.
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
- A model trail changes results on a base where nothing was ever judged, at half a
  person trail's weight. It changes none while the selectivity over the last 200
  labelled searches is over `label_select_max`.
- On a base with no judged search, `labels_count()` is true.
- Thirty-one unjudged decisions of which four are weak with an evidence note the
  second reading finds relevant move `decide_bears` down one step. A verdict on one
  of them replaces its second reading.
- A model trail whose note is opened from a search has `matched` 1 and a new
  `used_at`.
- Undoing the verdict retires the trail. Deleting captured searches deletes all.
- A model trail whose candidate is labelled under the ask line three times is
  retired; a person trail is not. A candidate brought by a model trail and labelled
  relevant makes no trail and no sweep pair.
- A matched note the reranker cut is in the results of a search at `Door::Ask`.
- A trail made by two verdicts survives the undoing of one and is retired by the
  undoing of both.
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
- `labels_count` turns round: trusted until shown otherwise. This changes a rule
  that is live today, for the sweep's existing model pairs too.
- A model trail counts half a person trail.
- An open confirms a trail.
- The second reading of unjudged decisions, and that it tunes the line beside
  verdicts.
- 200 labelled searches as the window for the selectivity check without a person.
- Opens and unjudged citations make no trail.
- The reorder formula, the ladder and the shipped weight of 0.5.
- The three thresholds for a trail's retirement (3 misses, more misses than matches)
  and the near-duplicate line (0.95).
- A verdict on a weak decision stamps no note and makes no trail.
- Use or a person confirms a trail; the model can only withdraw a model trail.
- A model trail ages `feedback.retain_days` after its last confirmation; a person
  trail does not age.
- A matched note survives the reranker's cut.
- The deep look ends at its first find.

## Left to the builder

- The interleave when one of the two searches returns fewer rows.
- How `Core` holds and refreshes the live trails and the cached `labels_count`.
- Where re-embedding of stale trails runs, and how many a pass.
- The order of work across the pull request, and its commits.
- The exact Insights wording, within the manifest's rules for copy.
