# Self-tuning, stage 3: the loop closes without a person

Written 2026-09-05. This replaces Part 5 of
`docs/superpowers/specs/2026-09-04-self-tuning-design.md` and the stage 3
paragraph of its "Order of work". Everything else in that spec stands.

## Why Part 5 is replaced

The goal was a base that tunes itself, deduplicates itself, reaps itself and
calibrates itself with nobody present. Part 5 said the corpus jobs "start by
proposing" and earn the right to act from the operator's responses. Read
against the tree at the end of stage 2b, that is the wrong shape twice over:

- **It needs a teacher.** Agreement between proposals and operator responses
  is measured over decisions a person makes. A base nobody supervises never
  earns anything, which is the opposite of the goal.
- **It is a step backwards.** Dedupe already merges, supersedes and discards
  on the model's verdict with nobody present; reap already buries; promotion
  and judgement already run inline. Part 5 would pull all of that back behind a
  queue.

Part 5 also does not match the tree: merge is a library dedupe calls, not a
job; promote and judgement have no threshold and no lane; the confident
consolidate lane orders which pairs are judged first and hides nothing on its
own; and reap's restore path exists — `Core::reactivate` exhumes from the
graveyard, over HTTP — but nothing lists what is in the graveyard, so nobody
takes it.

What replaces it applies the spec's first and third decisions to the corpus
side, the way stages 1–2b applied them to ranking. **The corpus jobs stay
autonomous. They answer to the same evidence the ranking side answers to, and
they take their own actions back when that evidence says so.** A person's undo
is the anchor, the way verdicts anchor ranking; it is never the primary signal.

The second decision of the original spec is restated: autonomy over the trace
rests on the corpus jobs' guards — originals kept, nothing lost on a score,
undo on everything — *and on the base checking its own corpus actions against
use*. Where an action has no undo path a person or the base can take, autonomy
does not follow. After this stage, every corpus action has one.

Three parts, three plans. Part A finishes the ranking side: the three knobs
stage 2b left out. Part B gives the corpus jobs a journal and two rules that
read it. Part C moves the corpus jobs' own thresholds on the same ladder the
ranking knobs walk.

## What this stage relies on

- `RankingParams` behind `Core::ranking`, carried by the live generation as
  JSON, widened with serde defaults (`src/core/ranking.rs`,
  `src/store/generations.rs`).
- The idle pass and its shape: quiet gate, one claim shared with the verdict
  sweep, the anchor suspension, the watch before any proposal, one knob per
  candidate, a tie keeps the current value (`src/jobs/tune.rs`).
- `sweep::rank_of`, `candidates`, `recommend`, `score` (`src/eval/sweep.rs`),
  `lived::holds_up` / `settled` (`src/eval/lived.rs`), `anchor::trustworthy`
  (`src/eval/anchor.rs`). None of them has a tuned constant, and nothing below
  adds one.
- Observations carry the query vector they were searched with, and their
  `rank` is the rank that was served — after the reranker, after priming,
  before the truncate (`src/core/search.rs`, the capture block).
- The schema doctrine's `ADDITIVE` allowlist in `src/store/mod.rs`: a nullable
  or defaulted column on an existing table, with a written reason. Every
  column below qualifies. The operator has said a recreated database is
  acceptable; the allowlist is used anyway because it costs nothing.

---

## Part A — The three knobs stage 2b left out

Stage 2b's "What the code admits" gave a reason for each. Each reason was
either a cost the operator has already accepted, a fact that a table can
record, or a measurement that had not been defined. None is a rule.

### A1. Rerank on or off

**What it is.** Where `[infer].rerank` names a reranker, every search on the
doors it applies to sends the candidate list to a model that re-sorts it. One
model call per search.

**Why 2b left it out.** Measuring "on" looked like a reranker call per pair per
candidate, and the gate scores ranking quality only, so it cannot weigh the
call every future search would pay.

**Why that does not hold.**

- The cost decision is already made. A reranker is in the pipeline because the
  operator wrote it into `[infer]`. The knob can switch the call *off* where it
  earns nothing, which saves a call and never adds one, and switch it back
  *on* only where the operator already consented to it.
- Measuring "on" is free where "on" is live. Observations are served with the
  reranker; the rank they record *is* the reranked rank. The rank without the
  reranker is exactly what the pass replays today.

**Design.**

- `RankingParams` gains `rerank: bool`. The file's starting value is `true`
  when a reranker is configured. When none is configured the knob is not on
  the ladder at all: there is nothing to switch on, and a generation naming
  `rerank: true` on a base with no reranker serves as `false` and says so in
  the log, the same way a failed rerank call already degrades to vector order.
- Serving reads the knob beside the existing per-request `rerank` flag and the
  role's `apply` scope: all three must allow it. The typing fast path and the
  Ask door's scope are unchanged.
- **Scoring.** The rerank axis has two rows, and its own base. The row for the
  live value is the *served* rank, read off the observation and costing
  nothing. The row for the other value is a replay: without the reranker it is
  the replay the pass already does; with the reranker it is one reranker call
  per pair at the live parameters, not per candidate. `recommend` is applied
  with the served row as base. Where the live value is "on", the candidate is
  "off" and adopts when the replay without the reranker places the pairs
  better; where the live value is "off", the candidate is "on" and adopts when
  the reranked order does. A tie keeps the current value, as everywhere.
- **What the replay with the reranker may spend.** At most one reranker call
  per pair, over at most `OBSERVATION_LIMIT` pairs, on the background lane,
  only when the live value is "off", and only because the operator configured
  the reranker. The pass stops between pairs when somebody comes back, as it
  does now.
- The rerank candidate joins the winner choice with the other candidates. One
  change at a time still holds: it is one more candidate on the ladder, and
  `BUDGET` grows by the one rung this axis has.
- An adopted rerank change is watched like any other adoption, by `lived`.

One consequence to state: the rows for the *other* axes keep measuring the
pre-rerank order where the reranker is live, as 2b said. The rerank axis is
the only one that compares served with replayed, because it is the only one
where the difference between them is the thing being measured.

### A2. `prime_lift`

**What it is.** A hit the operator has used often, has read in the current
sitting, or has a due reminder on may climb past hits above it, by at most
`prime_lift` places, never past rank 1, only when it beats each hit it passes
by `prime_margin`. Off the Ask and Judge doors, off any door without a
session, and shipped at zero.

**Why 2b left it out.** Priming reads the sitting, the due set and the
activation at the moment of the search. Observations record none of them, and
a column on `observations` was a recreated database.

**Why that does not hold.** A side table is free, and a nullable column is on
the additive list. The missing fact can be recorded.

**Design.**

- `RankingParams` gains `prime_lift: usize`, ladder `PRIME_LIFTS = [0, 1, 2,
  4]`. The file's `associate.prime_lift` is the starting rung. `prime_margin`
  and `sitting.prime` stay the operator's: the first is the definition of
  "more accessible", the second is consent for the sitting to move an order.
- **Capture.** A new table `search_context` — `event_id` (primary key,
  references `search_events`), `activation_json`, `sitting_json`, `due_json`
  — written beside the candidate pool on every captured search at a door that
  primes, whether or not the lift is currently above zero: a replay at lift 2
  from a base serving at lift 0 needs the context as much as the other way
  round. Off the request path, via `Background`, like the pool itself. The
  activation is the same `engagement_now` read priming already makes when the
  lift is above zero; at zero it is one extra store read per web search.
- `observations` gains a nullable `event_id`, stamped on opened and gave-up
  observations, both of which come from a search event. Cited observations
  come from the Ask door, which never primes, and carry none.
- **Replay.** The search takes its priming inputs as a value. Serving fills it
  from the live sitting, the due map and the activation read, as today; the
  pass fills it from `search_context` for the pair's event and hands it in on
  the Judge door, where priming is otherwise off. A pair with no context ties
  on the prime axis: for it, every rung is the same list.
- Cost: none beyond the capture. Priming reorders the replayed list and reads
  no vector.

### A3. Spread

**What it is.** Under the ranked list, the search appends up to `spread_max`
artifacts linked to the top `spread_from` hits. Additive: it never reorders or
removes a ranked hit. Off the Ask and Judge doors.

**Why 2b left it out.** The gate scores where a used hit stood. An appended hit
that was used can only help that score, and one that was not used costs
nothing the score can see, so a knob scored this way is turned to its maximum
and stays there. And nothing about the band reaches the evidence at all: the
band is appended after the pool is captured, and an open only counts when the
artifact is in the pool, so an opened appended hit is not an observation today.

**Design.**

- `RankingParams` gains `spread_max: usize`, ladder `SPREADS = [0, 1, 2, 3, 5,
  8]`. `spread_from` stays the file's: the evidence below says whether the
  band earns its place, not which anchors it should hang from.
- **Capture.** `search_candidates` gains `band INTEGER NOT NULL DEFAULT 0`.
  Appended hits are recorded in the pool with `band = 1`, at the ranks after
  the ranked pool, `shown = 1`. An open of an appended hit then stamps and
  writes an observation like any other, at the rank it was shown.
- **Scoring is lived, not counterfactual.** Whether a wider band *would* have
  been used is unknowable, the same way a give-up is. So the band is scored
  by what happened under the live generation, over its captured events that
  had a band:
  - `band_used`: events where the opened hit was in the band;
  - `tail_used`: events where the opened hit was in the last `spread_max`
    ranked positions that were shown — the band's own width, measured
    against the weakest ranked hits beside it.

  The band **grows** one rung when `band_used` exceeds `tail_used` by more than
  one event could account for, **shrinks** one rung when `tail_used` exceeds
  `band_used` by the same, and holds otherwise. That is `recommend`'s two-net
  rule with events for pairs. At `spread_max = 0` there is no band to
  measure, so the first rung is proposed the way any adoption is: once, when
  the ladder proposes nothing and nothing is under watch, and the band rule
  and the watch decide whether it stays. A rung taken back is a tried
  candidate and is not offered again while the models stand.
- A spread move is proposed only when the ladder proposes nothing: it is a
  different kind of evidence and does not compete on `predicted`. It is
  journaled as a generation with a parent and watched by `lived`, like every
  adoption. `predicted` for it is the band's use rate.
- Replay on the Judge door applies the band with today's links. Links only
  accumulate, so a replay sees at least what the search saw. Stated as the
  approximation it is.

### What the ladder looks like after Part A

Seven knobs: recency weight, per-source cap, pool depth, half-life, prime
lift, rerank, spread. Six on the counterfactual ladder — the first five and
rerank — and spread on its lived rule. `BUDGET` covers every rung of every
counterfactual axis, as 2b corrected it to. `moved` counts seven fields.

---

## Part B — The corpus jobs answer to the same evidence

### The journal

A new table `corpus_actions`:

| Column | Meaning |
|---|---|
| `id` | ULID |
| `at` | when the action was taken |
| `job` | `dedupe`, `consolidate`, `reap`, `promote`, `judgement` |
| `kind` | `merge`, `supersede`, `discard`, `stale`, `reap`, `promote`, `moment` |
| `subject_id` | the artifact (or moment) acted on — hidden, buried, created |
| `survivor_id` | for `merge` and `supersede`, what now answers for the subject; else null |
| `detail` | the judge's reason, or the rule's, as text |
| `evidence_json` | what the base knew at the time: the pair score, the observation count naming the subject, the age, the hit count |
| `undone_at` | when it was taken back, or null |
| `undone_by` | `operator` or `evidence` |
| `undone_reason` | text |

One row per subject. A merge of three originals is three rows sharing a
`survivor_id`. Every action site writes its row inside the same transaction
as the action, or immediately after it under the same lock where the action
is not transactional. Every existing undo route stamps `undone_by = operator`
on the row it undoes. Rows are never deleted; retention leaves them alone.

This is the record the ranking side has in `generations`, for the corpus: at
any moment "why is this hidden" and "what did the base do to itself" have an
answer.

### Two rules, one pass

Both run inside the idle pass, after the ranking half, under the same quiet
gate, the same `evolve.autonomous` switch, the same claim, and the same anchor
suspension. When observations no longer agree with verdicts, the corpus rules
act on nothing either; they read the same observations.

**Rule 1 — a survivor must still be found.** For every `merge` and
`supersede` not yet undone: take the observations that named the subject
before `at`, under any generation of the live era, replay them at the live
parameters, and read where the survivor lands. `satisfied_by` already resolves the subject to the survivor, so the
replay is the one the pass makes anyway. Compare the survivor's replayed
ranks with the subject's observed ranks by `recommend` pointed the other
way: when the subject's record is two net pairs better and loses on neither
aggregate, the action lost retrieval, and the base takes it back —
`merge::undo` or `unsupersede`, with `undone_by = evidence`. A row with no
observations naming its subject has no evidence and is left alone.

**Rule 2 — a give-up that a hidden artifact would have answered.** For every
give-up observation since the last pass: search its stored query vector once
more with hidden artifacts included, and, for the graveyard, by cosine over
the buried vectors. When the best *hidden* hit is more similar than the best
*live* hit in that search's captured pool, the hiding cost an answer. The
base restores it — `reactivate` for stale and discarded, `unsupersede` for
superseded, `exhume` for reaped — and stamps the row. A give-up says "this
list did not answer"; a hidden artifact that would have topped it is the
one thing the evidence names.

For rule 2 the graveyard keeps what it needs: `graveyard` gains nullable
`vec BLOB` and `embed_model`, written when a point is deleted. A buried
vector from another embedding era is skipped, as an observation from another
era is.

**Memory.** An action undone by evidence or by the operator is not repeated
on the same subject by the same job. `corpus_actions` is that memory, read by
the action sites before they act — the corpus side's `tried_candidates`.
Merges already have `artifact_sources.restored`; the journal generalises it.

**The anchor.** There is one, shared with ranking. Rule 1 and rule 2 read
observations, so `anchor::trustworthy` is the right suspension for them too.
The operator's undo is not a second anchor and does not compute an agreement
rate. It is recorded, it is disclosed beside the evidence undos, and it stops
the same action recurring. That is what "the operator's response is an
observation" reduces to once the primary signal is use.

### What joins, and how

| Job | Kind | Journaled | Rule 1 | Rule 2 | Restore path |
|---|---|---|---|---|---|
| dedupe | merge | yes | yes | yes (originals) | `merge::undo` |
| dedupe | supersede | yes | yes | yes | `unsupersede` |
| dedupe | discard | yes | — | yes | `reactivate` |
| consolidate | stale | yes | — | yes | `reactivate` |
| reap | reap | yes | — | yes | `exhume` via `reactivate` |
| promote | promote | yes | — | — | `undo_promotion` |
| judgement | moment | yes | — | — | the due band's undos |

Promote and judgement journal and nothing more. A promotion creates an
artifact; nothing in use says it should not exist — an artifact that is
listed and never used is most of any corpus. A wrong reminder is a fact about
a date, and use has nothing to say about dates. Both keep their operator undo
and their journal row, so the disclosure is complete even where the base
does not act.

### The graveyard becomes visible

Insights gains a **Reaped** section: what is in the graveyard, when it was
buried, the judge's reason, and a restore button on the existing reactivate
route. Capped like the other tables. This closes the "undo that exists as
data but not as a path" the original spec named, for the one person who
might want it; rule 2 is the path the base takes.

### Disclosure

The evolve section on Insights gains a folded list of the last corpus actions
with their undos, evidence and operator undos told apart, and a line for each
rule saying what it last did. Rendered for everyone who can open the page,
like the rest of the section. Nothing on it is a control except the restore
button above.

---

## Part C — The corpus thresholds move on the same ladder

A corpus job's threshold is a knob like the ranking knobs, with one
difference: the evidence about it is in the journal, not in a replay.

### The shape

Every threshold has two signals after Part B:

- **wrong** — actions beyond it that were taken back, by evidence or by the
  operator;
- **short** — what sits just beyond it and would have been right.

A threshold moves one rung when one signal clears the other by more than one
decision could account for, and holds on a tie. A threshold with only one of
the two signals does not move: a ladder that can only be walked one way is
not calibration, it is drift with a name.

### `review_min` moves

The one threshold with both signals today. Pairs are recorded with their
cosine, judged in `auto_supersede`-first order, and every judgement is a
verdict the journal sees.

- **Ladder:** `REVIEW_MINS = [0.80, 0.84, 0.88, 0.92]`, the file's
  `consolidate.review_min` as the starting rung. `auto_supersede` stays the
  operator's and the validation between them stays: a rung at or above it is
  not on the ladder.
- **Bands:** the pairs recorded in `[rung_i, rung_{i+1})` for each rung.
- **Short:** the lowest recorded band acts — merge, supersede, discard — as
  often as the band above it. Then the band below the threshold is expected
  to act too, and `review_min` steps down a rung. More pairs are then judged,
  at `max_dedupe_per_tick` per tick, which is the operator's bound on the
  model calls this spends.
- **Wrong:** the lowest band's actions are taken back, by rule 1, rule 2 or
  the operator, more often than the band above's. Then `review_min` steps
  up a rung.
- "As often as" and "more often than" are `holds_up`'s comparison of rates
  with one-decision noise, over the two bands' records.
- A move is journaled as a generation — the live generation carries
  `review_min` beside the ranking knobs, `Core` holds it behind the same
  lock, and `relate.rs` reads it there instead of the file — with the band
  records as `predicted`, and watched by the same comparison
  a pass later: the new lowest band holds up against the old one, or the
  rung goes back. One change at a time holds across ranking and corpus
  knobs alike: a base under watch for a `review_min` move proposes no
  ranking move, and the reverse.

### What does not move yet

`stale_after_days` / `stale_max_hits`, `activation_above`, and reap's
`min_age_days` each have a wrong signal after Part B — restores, unpromotes,
exhumes — and no short signal that costs nothing: knowing that a younger
artifact would have been safe to hide, or a quieter window right to promote,
means hiding or promoting it. They are journaled and disclosed. Each joins
the ladder when a short signal is defined for it, and the shape above is
what it has to fit. The stage stops here rather than inventing one.

---

## What stays with the operator

After this stage, a base with these four things in its file needs nobody:

- `evolve.autonomous = true` — the one switch, unchanged.
- a `[reap]` section — consent to bury, unchanged.
- `[infer].rerank` — consent to pay a model call per search; the base decides
  where it earns it.
- `sitting.prime = true` if the sitting may move an order — the base decides
  how far.

Everything shipped stays shipped as it is. The operator has the only
deployment and can turn the loop on; nothing here changes a default.

## Configuration

No new keys. The starting rungs come from keys that exist:
`associate.prime_lift`, `associate.spread_max`, `consolidate.review_min`, the
presence of `[infer].rerank`. The generation's `params` JSON widens with
`prime_lift`, `spread_max`, `rerank` and `review_min`, each with a serde
default equal to the file's shipped value, so rows written before this stage
still read.

Two stale sentences in `config.example.toml` are corrected on the way: the
`auto_supersede` comment that says the older artifact is hidden without
asking, and the `prime_lift` comment that says the feature waits on the
cargo harness. The idle pass is the harness now.

## Error handling

- A corpus rule that fails partway leaves every action it has not undone in
  place, and the next quiet period reads the journal again. An undo is one
  transaction and one journal stamp; there is no partial undo.
- A reranker that fails during replay makes that pair a miss on the "on" row,
  as a failed rerank degrades to vector order when serving; it does not end
  the pass.
- A `search_context` row missing for an opened observation ties the prime
  axis for that pair. A graveyard row with no vector is skipped by rule 2.
- Rule 2 restoring an artifact enqueues its embed, the way `reactivate` does
  today; a vector store that is unreachable ends the pass and the restore is
  retried next quiet period, because the journal row is still unstamped.
- Suspension covers both halves of the pass. Ops says so once.

## Testing

Sentences, as the suite writes them.

- The served rank is the rerank axis's base; a replay without the reranker
  that places two net pairs better adopts rerank off.
- A base with no reranker offers no rerank rung.
- A pair with a captured sitting ranks differently at lift 2 than at lift 0
  on the Judge door, and a pair without one ranks the same at every lift.
- An opened appended hit becomes an observation at the rank it was shown.
- A band used more than the ranked tail grows one rung; a tail used more than
  the band shrinks it; equal use holds.
- A spread move is proposed only when the ladder proposes nothing.
- Every corpus action site writes a journal row in the same transaction.
- A merge whose survivor ranks two net pairs worse than the original did is
  undone by evidence, and the pair is not merged again.
- A give-up whose best hidden hit beats its best live hit restores the hidden
  artifact; one whose best live hit is closer restores nothing.
- A reaped artifact is exhumed by a give-up and re-embedded; a buried vector
  from another era is skipped.
- An operator undo stamps the journal row and stops the same action on the
  same subject.
- An untrustworthy anchor stops both rules and the ladder alike.
- The lowest band acting as often as the band above steps `review_min` down;
  its actions being taken back more often steps it up; a rung at or above
  `auto_supersede` is never proposed.
- A base under watch for a `review_min` move proposes no ranking move.
- The Reaped section lists what is buried and the button exhumes it.

## Order of work

Three plans, in this order. Each is useful on its own.

1. **Stage 3a — the three knobs.** Part A. Ranking is complete after it:
   every knob the pipeline has is on the ladder or on its lived rule.
2. **Stage 3b — the journal and the two rules.** Part B, with the graveyard
   listing and the disclosure. The corpus jobs are self-checking after it.
3. **Stage 3c — `review_min` on the ladder.** Part C. Needs 3b's journal to
   have bands to read.

## Out of scope

- Threshold ladders for stale, promote and reap, until each has a short
  signal.
- `spread_from`, `prime_margin`, `auto_supersede`: the operator's definitions,
  not knobs.
- Tuning the judge's prompts or the embedding recipe; unchanged from the
  original spec.
- A second anchor computed from operator undos. One anchor, on the evidence
  both halves read.
- Serving exploration and cross-tenant learning; unchanged.
