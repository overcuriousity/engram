# Judgement by the decision model: judge first, then write

Date: 2026-11-13. Spec 2 of 3. Depends on the foundation in spec 1
(`2026-11-13-decide-1-verb-design.md`): the `[infer.decide]` role, `Decider`,
swapped reads, the budget. Highest priority of the three.

File and line references come from a read of `522f26aa`; verify before relying.

## Intent

engram refines itself in the background: it merges duplicates, retires what was
replaced, keeps disagreements, links what belongs together, buries what is
worthless. Every one of those is a *judgement*, and today a chat model makes
each of them by writing JSON, one slow call at a time, with no measure of how
sure it was. Because the calls are expensive they are rationed: five dedupe
judgements per tick, ten link judgements per sweep, twenty reap judgements a
day, and only pairs above 0.88 cosine are looked at at all.

A decision model makes the same judgement in tens of milliseconds and returns a
probability with it. Two things follow.

- **Judgement leaves the chat model entirely.** The chat model keeps the one
  thing only it can do, writing text, and writes only after a decision says
  the text is wanted and only if a second decision says the text is faithful.
- **Judgement stops being rationed.** The base can afford to look at far more
  of itself, and a probability on every judgement means it can act when sure,
  ask the person when torn, and leave the rest alone.

The owner's decision: this replaces the autonomous judging completely. There
is no chat-model fallback for judgement.

## Where judgement and writing live today

Thirteen call sites. What happens to each:

| Call | Today | After |
|---|---|---|
| `jobs/dedupe.rs:219` pair relation + merged text | chat, one call | **decision**; merged text by the writer only on `duplicate` |
| `jobs/associate.rs:335` link verdict + reason | chat | **decision**, typed link kinds; no text |
| `jobs/reap.rs:395` worthless / valuable + reason | chat | **decision** |
| `jobs/window.rs:93` synthesis, incl. category, pinned, intent, links | chat | writer keeps the artifacts and moments; **category, pinned, intent, links by decision** |
| `jobs/dedupe.rs:355` operator-asked merge | chat | writer, then checked |
| `jobs/reap.rs:200` rescue | chat | writer, gated and checked |
| `jobs/condense.rs:86` condense | chat | writer, then checked |
| `jobs/pursuit.rs:109` generate | chat | writer, gated and checked |
| `jobs/synthesize.rs:163` title, `jobs/gaps.rs:313` gap label, `jobs/describe.rs:33` image | chat | unchanged |
| `core/ask/plan.rs:34`, `core/ask/mod.rs:281` | chat, interactive | unchanged, out of scope |

## Goals

- No background judgement is made by a chat model.
- Every judgement is a recorded decision with its probabilities, the band it
  fell in, what was done, and what became of it.
- Three bands per question: **act**, **ask**, **leave**. Thresholds start
  conservative and move with the person's undos and answers.
- Every background writer call is preceded by a decision that it is wanted and
  followed by a decision that the result is faithful.
- The base judges far more of itself: a wider pair net that finds
  contradictions and replacements similarity alone misses, typed links,
  per-artifact readings, and relevance labels on recorded searches.
- With an embedder and Clef-flash but no chat model, every judgement still
  runs; only the writing is absent, and says so.

## Non-goals

- Ask and its planner. Titles, gap labels, image description.
- Replacing heuristics that need no judgement (shingle near-duplicates at
  capture, containment supersede in `jobs/relate.rs`, activation, recency).
- Fine-tuning Clef-flash.
- User-written questions (spec 3).

## Anti-goals

- **Two judging backends.** The chat judging prompts, schemas and parsers are
  deleted, not kept behind a switch.
- **A chore list.** The ask band is capped and lapses. A base that asks more
  than it acts has its thresholds wrong, and the tuner must fix that rather
  than the person.
- **A single probability trusted as truth.** Nothing irreversible rests on one
  call. Acting needs a swapped read that agrees, the daily caps stay, and
  every action keeps its undo.
- **Model labels overruling the person.** A person's verdict always wins, and
  model-made relevance labels steer ranking only while they demonstrably agree
  with the person's.
- **Quietly judging on cut-off text.** A case that does not fit the context is
  left, counted and shown, never judged on a truncated member.

## Constraints

- 8192 tokens per call as served; the budget is `decide::budget` from spec 1.
- `[learn] mode`, `Autonomy`, `evolve.max_actions_per_day` and
  `taken_back_before` gate actions exactly as today.
- Every action keeps its journal row and undo (`store/actions.rs`).
- A change of decision model or of a question's wording is a new *era* for
  that question: earlier evidence about its thresholds stops counting.
- The manifest; `.sqlx/` regenerated after schema changes.

## Approaches

1. **Swap the completer.** Keep each stage's shape and replace its chat call
   with a decision call. Smallest change, but leaves thresholds, review and
   recording to be invented per stage, and adds nothing new.
2. **One decision layer that every stage goes through (chosen).** A catalogue
   of versioned questions, one record of every decision, one banding and
   tuning rule, one "unsure" surface. Stages become: build the state, ask,
   apply the band. New refinement is then a new question, not a new subsystem.
3. **A separate refinement service.** Rejected: engram is one process.

The owner chose "judge first, then write" over a background that never writes.
The layering is the designer's pick.

## The decision layer

### Questions

A built-in question is code: an id, a version, a type, its instructions and
criteria, and a function from the subjects to a state. Wording is behaviour, so
a change of wording bumps the version.

### The record

One table, `decisions`:

- id, question id and version, model, era
- subject_a, subject_b (artifact, pair, link or search-candidate ids)
- answer JSON (probabilities as returned), top probability, pick
- swapped_agree (null when not a choice or not swapped)
- band: `act`, `ask`, `leave`, plus `leave` reasons `too_long`, `capped`,
  `gated`
- action_id (the journal row, when it acted)
- outcome: null, `kept`, `undone`, `confirmed`, `corrected`; outcome_at
- decided_at

An action's undo writes `undone` here. A person answering an ask-band row
writes `confirmed` (they picked the model's pick) or `corrected`.

### Bands

For a question with thresholds `act_at` and `ask_at`, on the top probability:

- **act**: top `>= act_at`, and for a `choice` the swapped read agreed, and
  the existing gates allow the action. Otherwise it falls to ask.
- **ask**: top `>= ask_at`. The decision is shown to the person.
- **leave**: below. Recorded, nothing happens, the subject is asked again
  only when it changes or a new era begins.

Starting values, all in config:

```toml
[decide.bands]
act_at = 0.90
ask_at = 0.60
# Per-question overrides.
[decide.bands.reap_worth]
act_at = 0.97
```

### Tuning

`jobs/tune.rs` already moves `review_min` from undone and acted rates. It
gains one rule per question and era:

- With at least `decide.tune_min` (30) acted decisions with an outcome window
  closed (30 days, or an explicit outcome), if the undone share exceeds
  `decide.undo_target` (0.05), raise `act_at` one step; if it is under half
  the target and the ask band is non-empty, lower it one step. Steps of 0.02,
  bounded to `[0.80, 0.99]`.
- `ask_at` moves so that the open ask rows stay under the cap: raised when
  rows lapse unanswered, lowered when the person clears them.
- Every move is a row in the tuner's existing record, and shown on Insights.

### The unsure surface

On Insights, above "what the base did on its own":

```
Unsure · 4
  Same thing?   Mount loop device  ↔  losetup notes
                duplicate 48 · distinct 41 · replaced 9 · conflict 2
                [Duplicate] [Replaced ←] [Replaced →] [Conflict] [Distinct] [Leave]

  Worth keeping?   Old nginx rewrite rule (retired 2026-06-02)
                valuable 64 · worthless 36
                [Valuable] [Worthless] [Leave]
```

- A row names the question in two or three words, the subjects (each opens),
  the weights, and one button per answer. A pick does what the act band would
  have done, attributed to the operator, and has the same undo.
- At most `decide.ask_open_max` (20) rows are open. A decision that would
  exceed it is `leave` with reason `capped`. A row unanswered for
  `decide.ask_lapse_days` (30) lapses to `leave`.
- Open contradictions keep their existing pair view; this surface is for the
  torn, not for the conflicting.
- The count joins the status line and `/api/v1/status`.
- JSON siblings under `/api/v1` as `src/web/judge.rs` does for the journal.

### Throughput

Decision stages are not `GENERATORS`: they are not held back when generation
is shut and do not wait out `pacing.cooldown_secs`. They share one budget,
`decide.max_per_tick` (200), and yield to any interactive lease. The existing
per-stage caps (`consolidate.max_dedupe_per_tick`, `associate.judge_per_sweep`,
`reap.max_judged_per_run`) stay as keys, now capping decisions, with raised
defaults (50, 100, 200). Writer caps (`reap.max_rescues_per_run`, the cooldown)
are untouched.

### When something does not fit

Context around the subjects (merge sources, neighbours, cues) is dropped first,
oldest first. The subjects themselves are never cut. If they do not fit, the
decision is `leave` / `too_long`; Insights shows the count. The builder may
later judge long members passage by passage; not in this spec.

### When the role is absent

Dedupe, link judging, reaping, capture classification and everything in
"New refinement" do not run. Each says "not configured" where it would have
appeared, as the embed-only floor does for synthesis, and boot logs one line.
The owner accepts that a base without `[infer.decide]` stops judging.

## The questions

Wording below is a first draft; the builder tunes it against the replay
(stage 2) and records the final text in code.

### `pair_relation`: choice, swapped (replaces `DEDUPE_SYSTEM`)

State: both artifacts with title and capture date, then merge sources while
they fit. "How do A and B relate?"

- `duplicate`: they state the same thing; nothing in one is missing in the other's subject
- `a_replaces_b`: A is newer or corrected and makes B outdated
- `b_replaces_a`: the reverse
- `conflict`: they give different values or instructions for the same thing and neither is clearly newer
- `distinct`: different subjects, or the same subject with different content worth keeping apart
- `vacuous`: neither states anything a reader could use

Actions as today (`dedupe.rs:461`): distinct closes the pair; conflict opens a
contradiction for the person; vacuous discards both; a replacement supersedes;
duplicate goes to the writer for a merge (`SYNTHESIZE_SYSTEM`, which already
takes the relation as settled). The existing guards stay: newest-wins,
loss checks, `taken_back_before`.

- No writer configured: a `duplicate` in the act band becomes an ask-band row
  whose `[Duplicate]` button is replaced by `[Keep A] [Keep B]`.
- The one-sentence `detail` is gone. The pair view already shows both sides.

### `link_kind`: choice, swapped (replaces `LINK_SYSTEM`)

State: both artifacts and the search cues that bound them. Options:
`continues` (one is the next step after the other), `example` (one is an
instance of what the other describes), `alternative` (two ways to the same
end), `background` (one explains what the other uses), `contradicts`,
`duplicate`, `unrelated`.

- The reader's line is rendered from the kind ("An example of this", "Another
  way to do this"); no model text.
- `duplicate` and `contradicts` hand the pair to consolidation, as `duplicate`
  does today. `unrelated` shelves the link.
- `continues`, `example` and `background` are directional: a second question
  in the same request, `link_direction` (choice: `a_first`, `b_first`), settles
  the order.

### `reap_worth`: noul (replaces `REAP_SYSTEM`)

State: the retired artifact, its successor, up to five live neighbours. "Does
the retired note state anything a reader could use that the live notes shown do
not?" Yes at `act_at`: valuable, to the rescue writer. No at `act_at` (0.97,
since burial is the one action that wipes text): bury. Between: ask.

### Capture readings: one request per artifact

Asked of every new artifact on both capture paths, and of existing ones in the
idle pass (see "The scan").

- `category`: choice over the eight `CATEGORIES`. Replaces the synthesizer's
  field; also gives verbatim passages a category, which they lack today.
- `pinned`: noul, "Is this something to keep permanently visible: a
  standing rule, a credential location, an emergency procedure?" Acts only as
  today's `pinned` does; `act_at` applies.
- `asks_reminder`: noul, "Does this ask to be reminded of something?"
- `has_date`: noul, "Does this mention a specific future date or deadline?"
- `time_bound`: noul, "Does this state something that will go out of date: a
  version, a price, a schedule, a temporary workaround?"
- `self_contained`: noul, "Can this be understood without the document it
  came from?"

The synthesizer's schema loses `category`, `pinned` and `links`. The judged
schema (moments, `when`, RRULE) is requested only when `asks_reminder` or
`has_date` is in the ask band or above on the window; otherwise the plain
schema is used. Extracting the time itself stays with the writer: a decision
model cannot write a date. A forced intent from the person overrides as today.

Links that the synthesizer used to propose are replaced by `link_kind` asked
between each new artifact and the neighbours it was shown.

### Writer gates

Before a writer call:

- **Merge:** `pair_relation` = duplicate (above).
- **Rescue:** `reap_worth` = valuable (above).
- **Generate** (`jobs/pursuit.rs`): `answerable`, noul, state the questions
  and the packed sources, "Do these excerpts contain the answer to these
  questions?" Below `ask_at` the pursuit closes unsatisfied without a call;
  between, it waits for more sources.
- **Promote** (`jobs/promote.rs`): activation as today **and**
  `self_contained` below 0.5. A passage that is used and already stands alone
  is not rewritten.

After a writer call, for merge, rescue, condense, generate and promotion, one
request with two `noul` questions, state "Original(s)" then "Rewrite":

- `complete`: "Does the rewrite state everything the originals state?"
- `adds_nothing`: "Does the rewrite state only what the originals state?"

Both at `act_at` or the write is refused exactly as a lost literal is refused
today (`merge::losses`, `verify::missing_numbers`, which stay). A refusal is a
decision row with outcome `refused`. For generate, only `adds_nothing` applies.
A pair that does not fit is `too_long` and the literal checks alone decide, as
today.

Capture synthesis is checked per artifact against its window with
`adds_nothing` only, where both fit.

## New refinement

### A wider pair net

Today only neighbours at or above `consolidate.review_min` (0.88) reach a
judge. Contradictions and replacements usually sit lower: two notes about the
same port with different numbers are not near-duplicates.

- New `consolidate.scan_min` (0.70): pairs down to it are put to
  `pair_relation`.
- Below `review_min`, nothing acts on its own. `conflict` at `act_at` opens a
  contradiction as today; `duplicate` and replacements go to the ask band;
  `distinct` and everything under `ask_at` are recorded and left.
- `per_point` stays the fan-out. A pair is asked once per era and again when
  either member changes.

### The scan

The idle pass asks the capture readings of every live artifact that lacks them
for the current question versions, newest first, inside `max_per_tick`.
Answers live in `artifact_readings` (artifact_id, question id and version,
answer JSON, model, at). Uses:

- `time_bound` joins the stale test in `core/search.rs`: an artifact is a
  stale candidate when it is old, unused **and** time-bound, or as today when
  the reading is missing. Detail pages mark a time-bound artifact older than
  `stale_after_days` "may be out of date".
- `self_contained` feeds promotion (above).
- `category` fills what is empty; it never overwrites a person's choice.

Spec 3 stores user questions in the same table.

### Relevance labels

For each recorded search (`feedback.candidates` are stored with it), each
candidate is asked `answers_query`, noul, state the artifact, "Would this note
help someone who searched for: «q»?"

- Stored as verdicts with a new `Labeller::Model`, never overwriting and
  always overridden by the person's.
- Insights shows recall@10 and MRR twice: by the person's verdicts and by the
  model's.
- The sweep (`eval/sweep.rs`) may count model labels only while, over at
  least `decide.label_overlap_min` (30) candidates judged by both, they agree
  with the person at `decide.label_agree_min` (0.85) or better. Below that
  they are display only. The agreement figure is shown.
- The same question replaces the similarity test where a gap is closed
  (`jobs/gaps.rs` `cover`) and where a pursuit counts as answered
  (`pursuit.rs` `answered_since`): a gap closes when a new artifact answers
  its question at `act_at`.

## Removed

`DEDUPE_SYSTEM`, `LINK_SYSTEM`, `REAP_SYSTEM`, their schemas, prompt builders
and parsers; `Core::judge`, `link_judge`, `reaper`; the `(attempt N)` cache
defeat and the unreadable-judgement counters (a decision either parses or is an
error to retry); `category`, `pinned`, `links` in the synthesizer schemas and
in all eleven language prompts.

## Stages

Each ships alone, in this order.

1. **The layer.** `decisions`, bands, config, the record of outcomes from
   undo, the unsure surface with no producers yet, tests with
   `ScriptedDecider`.
2. **Replay.** `engram --replay-decisions --user S [--question ID]` runs the
   new questions over what the journal and the pairs already hold (past
   merges, supersedes, contradictions, discards, burials, link verdicts, each
   with whether it was undone) and prints, per question, agreement with the
   past verdict, agreement with the person where they undid or confirmed, and
   the share that would fall in each band. Read-only. Its output goes into
   this file under "Measured", and wording and thresholds are fixed from it
   before anything acts.
3. **Dedupe, links, reap** move; the chat judging code is deleted.
4. **Capture readings** and the synthesizer schema cut.
5. **Writer gates**, before and after.
6. **Wider pair net.**
7. **The scan** and its uses.
8. **Relevance labels.**
9. **Tuning** of `act_at` and `ask_at`.

## Left to the builder

- Module layout; whether `decisions` and `artifact_readings` share code.
- Final wording of every question, fixed by the replay.
- How a link's direction is stored and rendered.
- The migration of open pairs and links that were mid-judgement.
- How `Insights` lays out the unsure block on a phone.
- Whether `time_bound` needs a Qdrant payload field or SQLite suffices.

## Calls made by the designer

- One decision layer rather than per-stage swaps.
- The three bands, their starting thresholds, the caps and the lapse.
- Swapped agreement as a condition for acting on any `choice`.
- `pair_relation` splits `replaced` into two directed options instead of a
  second question.
- The seven link kinds, and that the reader's line is rendered, not written.
- `detail` and `reason` free text are dropped, not replaced.
- The post-write checks, and that they add to the literal checks rather than
  replace them.
- `scan_min` 0.70 and "nothing acts on its own below `review_min`".
- Model relevance labels count toward sweeps only above a measured agreement.
- The judged synthesis schema is requested only when a reading calls for it.

## Done when

- `grep` finds no chat completion call in `jobs/dedupe.rs` judging, in
  `jobs/associate.rs` or in `jobs/reap.rs` judging, and the three prompts are
  gone.
- Every action in the journal that came from a judgement links to a decision
  row, and undoing it sets that row's outcome.
- With a scripted decider answering 0.99 on a choice whose reversed read
  disagrees, nothing acts and an unsure row appears.
- With `[infer.decide]` set and no `[infer.synthesize]`, a duplicate pair ends
  as an unsure row with keep-one buttons and no chat call is attempted.
- A writer result that fails `adds_nothing` is not stored.
- The replay has run on the owner's base and "Measured" is filled in.
- The checks in `AGENTS.md` pass.

## Measured

Empty until stage 2 has run.
