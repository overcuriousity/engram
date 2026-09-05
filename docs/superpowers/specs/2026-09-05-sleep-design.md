# Sleep: the base integrates, rehearses and reorganises while nobody is there

Written 2026-09-05, at the end of self-tuning stage 3b. It stands on
`docs/superpowers/specs/2026-09-04-self-tuning-design.md` and
`2026-09-05-self-tuning-stage-3-design.md`, and on the handoff at
`docs/superpowers/plans/2026-09-05-self-tuning-handoff.md`. Nothing in either
spec is replaced; this adds a source of evidence they did not have and the
things that source makes safe.

## Why

An evaluation of the tree at the end of 3b found two gaps that turned out to
share a root.

**The loop is capable and switched off.** `evolve.autonomous` and
`evolve.feed_sweep` ship `false`, and the reason given is sound: a default that
changes ranking moves only after the harness has been run. But the guard the
loop leans on is weaker than it reads. `anchor::agreement` returns `None`
where no judged search has an observation beside it, and `tune::pass` then
*skips the check and proceeds*. On a base nobody judges — which is most bases,
most of the time — the loop runs with its one safeguard absent. That is not a
guard that can be defaulted on.

**The plasticity is thin where the biology is rich.** Activation is one
scalar, and it measures frequency of use times decay. There is no salience —
nothing distinguishes what mattered from what was touched. There is no
interference — nothing is forgotten because something else won; `reap`
nominates on age. There is no offline replay — consolidation is triggered by
embed events and clocks, never by the base re-presenting anything to itself.
And `promote` writes a synthesis once, at a threshold, and never revisits it.

The root: **the base cannot produce its own evidence.** Every signal it learns
from — an open, a confirmation, a citation, a second search — is a human act.
Without one, the anchor is inert, the watch never settles, interference has no
input, salience has no measurement, and nothing is safe enough to default on.

What follows gives the base a source of evidence that costs no human and no
synthetic text, and then builds on that source exactly the four things the
evaluation found missing. The frame is sleep: a quiet base integrates the
day, rehearses what it holds, reorganises on what rehearsal shows, and says
in the morning what it did.

## The five decisions this rests on

1. **The corpus tests itself with its own later contents.** A capture is a
   query written by someone who was not looking at the answer. When a new
   artifact lands, at integration, near an older one from another corpus, the
   new text is an honest probe for the old — independently worded, by a
   person, at another time. That is the standard `docs/evaluation.md` holds
   verdicts to, and it is met without one synthetic line. No probe is ever
   minted from an artifact's own title or body: that is the "test query
   written while looking at the answer" the README refuses, and refusing it
   is right.

2. **Salience is surprise at integration, computed once.** What the base
   finds when a new artifact is run against the rest of it — nothing near,
   something near that agrees, something near that disagrees — is written
   down as that artifact's tag. One read, at write time, from a search that
   already runs. Not a variance over replay rounds; a word a person
   understands: *new*, *known*, *conflict*.

3. **The artifact layer becomes versioned-plastic.** The corpus stays
   immutable. An artifact's text may be rewritten by the base when the
   evidence says so, and every prior version stays stored, readable and one
   call from live. The second rule of the README is restated, not broken:
   from *a captured artifact is never rewritten in place* to *nothing is
   lost, and every step is readable and reversible*. The README carries the
   new wording after this stage.

4. **Autonomy is staged, and the reversible half is the default.** Moving a
   ranking generation is fully reversible and bounded; rewriting an artifact
   or burying one is less so. `evolve.autonomous` gains a middle stage that
   permits the first and not the second, and that stage is the new default.
   Defaulting on what can be taken back at no cost is a stronger argument
   than flipping one bool that brings the destructive half with it.

5. **Absent evidence never passes a gate.** Where neither human verdicts nor
   rehearsal has anything to say, the pass does nothing and Insights says so.
   The `None` that walks past the anchor today is the one defect this spec
   exists to remove, and every gate below is written so that it cannot recur.

And one refusal, on the record with the README's others: **no schema
artifacts from clusters.** The obvious biological continuation — write one
artifact that stands for a cluster of strongly linked ones — is the "digest
competing with the wording it was derived from" that the README already
refuses. Condensing one artifact into a shorter version of itself, which
supersedes only itself, sits inside the `promote` precedent. A cluster schema
does not, and it is not built here.

Nor does the base gain drives. Sleep is metabolism, not will: it orders what
the day brought and asks no questions of its own. Curiosity, coverage
pressure and self-posed queries were considered and refused — model calls
without a measured retrieval gain, against the third rule, and against the
roadmap item that wants a first run with *fewer* services.

## What this relies on

- `VectorStore::neighbours(id, limit)` addresses a point by id and pays no
  embedding; `dense_of(id)` returns the stored vector (`src/vector/mod.rs`).
- `sweep::Pair { query, satisfies, query_vec, priming, served }` and
  `sweep::rank_of(core, pair, params, rerank)` replay a stored vector under
  any `RankingParams` on the Judge door, embedding nothing
  (`src/eval/sweep.rs:239`, `:265`). A probe is a `Pair`.
- `eval::satisfied_by(core, id)` widens an expected artifact to what has
  superseded it (`src/eval/mod.rs`). This is what makes lineage a property of
  every probe rather than a probe class of its own.
- `infer::facts::fact_tokens(text)` and `PairState::Contradiction` — the
  dedupe queue already has a fact-token prefilter and a state for a
  disagreement (`src/infer/facts.rs:114`, `src/store/pairs.rs:41`).
- `jobs::merge::losses(roots, draft)` — every value and machine literal a
  draft would lose (`src/jobs/merge.rs:309`).
- `links::engagement_at` — activation above the decayed capture baseline
  (`src/store/links.rs:209`), what `promote` reads.
- The idle pass: quiet gate, one claim shared with the verdict sweep, the
  anchor, the watch, one knob per adoption (`src/jobs/tune.rs`).
  `lived::holds_up` / `settled` (`src/eval/lived.rs:73`, `:92`).
- The corpus journal and its two rules (`src/store/actions.rs`,
  `src/jobs/retract.rs`); `supersede_with` writing the row in the same
  transaction as the hide.
- `relate` runs only for model-written artifacts, because neighbours under
  one heading are similar for structural reasons (`src/jobs/embed.rs:336`).
  Integration inherits that reasoning in a different form: it discards
  same-corpus hits rather than skipping passages.
- `Cursor { at, id }` in `src/store/mod.rs`, and the wrap-at-lap pattern of
  `retract`'s `acted_after`.
- The schema doctrine: new tables are free; a new column on an existing
  table is on the `ADDITIVE` list or recreates the database. Every table
  below is new; no column is added.

---

## Part 1 — Integrate: the day is filed

The first phase of a sleep. Every artifact indexed since the last sleep is
run against the rest of the base, once, and what comes back is written down
as that artifact's tag and as probes for what it landed on.

### 1.1 What runs

`jobs::sleep::integrate(core, started)` walks `artifacts` on a cursor
`(created_at, id)` in meta `sleep.integrated_after`, bounded per pass by
`OBSERVATION_LIMIT` (500, the bound the ladder already uses), stopping
between artifacts when `activity_since(started)` says somebody came back. A
base offline for a month catches up over a few sleeps, the way `associate`
catches up.

For each artifact `A` that is in results and has `embed_state = 'ready'`:
`core.vectors.neighbours(A.id, limit)` with the limit `relate` uses. Hits
from `A`'s own corpus are discarded — that is the structural similarity
`relate` skips passages for, removed by filter rather than by exclusion, so
passages are integrated too. Hits that are not in results are discarded.

### 1.2 The tag

Read off the nearest surviving hit `H` and its cosine `s`, against two
thresholds that already exist and are already on the ladder:

| Condition | Tag |
|---|---|
| no hit, or `s < consolidate.review_min` | `novel` |
| `s >= review_min`, and not a conflict | `known` |
| `s >= consolidate.auto_supersede` and `fact_tokens(A) ≠ fact_tokens(H)`, both non-empty | `conflict` |

The conflict rule is the narrow one on purpose. At or above
`auto_supersede` the two texts claim to be the same statement; if they carry
different values while claiming that, one of them is wrong or out of date,
and deciding which is the judgement the README says a model is worst at. A
`conflict` files an `artifact_pairs` row in state `Contradiction`, for a
person, with the differing tokens in its detail. Below `auto_supersede` a
value difference is two notes about two things and is `known`.

A `known` artifact files no dedupe pair here. `relate` already does that for
model-written artifacts, and for passages the stage 3 reasoning stands:
duplicate detection over verbatim text waits until use promotes it.

Written to a new table:

```sql
CREATE TABLE IF NOT EXISTS integrations (
  artifact_id   TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,
  at            INTEGER NOT NULL,
  -- novel | known | conflict
  tag           TEXT NOT NULL,
  nearest_id    TEXT,
  nearest_score REAL,
  -- For a conflict: the fact tokens each side carries that the other does not.
  detail        TEXT
);
```

One row per artifact, written once. A re-embed does not re-integrate: the
tag is what the base knew when the artifact arrived, which is the meaning of
surprise. An artifact that was `novel` and later gets a neighbour stays
`novel`; that later neighbour is `known`, and the probe it writes (1.3) is
how the two are connected.

### 1.3 The probes this writes

For every surviving hit `H` with `s >= review_min` — the `known` and
`conflict` hits — one probe:

```
class = 'capture', query = A.text, query_vec = dense_of(A), artifact_id = H, source_id = A
```

`A` is a question somebody asked, in their own words, that `H` answers. The
probe belongs to `H`. It is what Part 2 replays, and it is the one class of
evidence in the system that is both human-worded and human-free.

`A.text` is stored on the probe rather than joined, so a later condensation
of `A` (Part 4) does not rewrite the question that was asked.

---

## Part 2 — Rehearse: the base checks what it holds

### 2.1 The table

```sql
CREATE TABLE IF NOT EXISTS rehearsals (
  id           TEXT PRIMARY KEY,
  created_at   INTEGER NOT NULL,
  -- capture | cue
  class        TEXT NOT NULL,
  query        TEXT NOT NULL,
  query_vec    BLOB NOT NULL,
  vec_dim      INTEGER NOT NULL,
  embed_model  TEXT NOT NULL,
  -- The artifact this probe is for.
  artifact_id  TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- capture: the artifact whose text is the query. cue: NULL.
  source_id    TEXT,
  -- Set when the owner leaves results for good (reaped) or the probe's model
  -- is no longer the live embedder. A retired probe is not replayed and not
  -- counted; nothing deletes it.
  retired_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_rehearsals_owner ON rehearsals(artifact_id);
CREATE INDEX IF NOT EXISTS idx_rehearsals_lap   ON rehearsals(created_at, id) WHERE retired_at IS NULL;
```

Two classes:

- **`capture`** — written by Part 1. The strongest: human-worded,
  independently timed, and never derived from the owner.
- **`cue`** — each entry of `artifacts.cues` on a model-written artifact.
  Written by the model at synthesis, *for* the artifact and not *from* it.
  Weaker than a capture probe, and the only class that costs an embedding.

There is no `lineage` class. A merge or a supersession is already a property
of every probe: `Pair.satisfies` is `satisfied_by(owner)`, so a probe whose
owner was merged away is satisfied by the survivor, the way a verdict naming
a merged artifact already is. Nothing has to be written for that.

And one derived state that is not a row: **unrehearsed**. An artifact in
results with no live probe — nothing has landed on it, it has no cues, and
(reading `observations`) nobody has opened it from a search. The base knows
which of its memories have never been asked for. Insights lists the count
and the list; nothing else reads it in this stage.

### 2.2 Minting cue probes

At `embed::mark_indexed`, for a model-written artifact with cues, one unit
is armed — `Stage::Probe`, its own failure domain, for the reason `relate` is
its own unit: a failing embed of the cues must not fail the embed of the
artifact. The unit embeds every cue in one batch call and inserts one row
per cue. Idempotent on `(artifact_id, class, query)`.

This is the one inference this spec adds to the write path, and it is an
embedding, not a generation, on artifacts that already cost a generation to
write. A base with no chat model has no cues and mints no cue probes; its
rehearsal is capture probes only, and Insights says so.

### 2.3 The replay

`jobs::sleep::rehearse(core, live, started)`. Pure vector reads, zero
inference: `rank_of(core, &pair, live.params, false)` with
`Pair { query, satisfies: satisfied_by(owner), query_vec: Some(vec), priming: None, served: None }`.

Order of work in one pass, both halves bounded together by
`OBSERVATION_LIMIT`:

1. **Fragile first.** Probes whose last two results under the live
   generation disagree — found then missed, or moved rank — up to half the
   bound. What wobbles is what needs rehearsing; this is the spacing effect,
   measured rather than scheduled.
2. **The lap.** The rest of the bound from the cursor `(created_at, id)` in
   meta `sleep.rehearsed_after`, wrapping at the end, as `retract`'s
   `acted_after` does. A probe has no end state to reach, so the cursor has
   no end.

Between probes: `activity_since(started)` → stop, cursor saved, nothing half
written.

Skipped, not failed: a probe whose `embed_model` is not the live embedder
(the way `retract`'s rule 2 skips give-ups from another era — it is retired
at that moment), and a probe whose owner is no longer in results and has no
survivor (retired likewise).

### 2.4 What a replay writes

```sql
CREATE TABLE IF NOT EXISTS rehearsal_results (
  id            TEXT PRIMARY KEY,
  rehearsal_id  TEXT NOT NULL REFERENCES rehearsals(id) ON DELETE CASCADE,
  generation_id TEXT NOT NULL REFERENCES generations(id),
  at            INTEGER NOT NULL,
  -- 1-based, like observations.rank; NULL past LIMIT.
  rank          INTEGER,
  -- JSON list of artifact ids that stood above the owner, in order. Empty
  -- when rank = 1; the full top LIMIT when the owner was not found.
  outranked_by  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rehearsal_results_probe
  ON rehearsal_results(rehearsal_id, at DESC);
```

Retention: the retention unit that expires observations past
`feedback.retain_days` expires these on the same clock. What Part 4 reads is
"every retained result", so the window is the operator's one retention
setting and not a second number.

---

## Part 3 — Calibrate: the anchor that cannot be inert

Rehearsal is a **yardstick, not a goal**. It may refuse a candidate and it
may revert a generation. It never adopts on its own: a ranking tuned to find
artifacts by the wording of later captures is a ranking tuned to something
one step removed from a person's question, and adopting on it alone would
optimise a proxy. Adoption stays on observations, where use is the evidence.
What rehearsal adds is that no adoption, and no live generation, can escape
being measured.

### 3.1 The record

```rust
pub struct Rehearsed { pub probes: usize, pub found: usize, pub mrr: f64 }
```

`rehearsed_under(core, params, probes)` replays a set of probes under
`params` and returns the record — counterfactual, on the current corpus, so
two generations are compared on one corpus and one probe set with nothing
else moving. The set is the probes with a retained result under the live
generation (the ones the lap has reached), bounded.

The noise term is one probe's worth: a single probe moving between rank 1
and a miss moves MRR by `1 / probes`. Same shape as `lived`'s
`one_observation`; no tuned number.

### 3.2 Three changes to `tune::pass`

**The anchor.** Today:

```
if let Some(a) = agreement && !trustworthy(a) { return }   // None walks past
```

Becomes: read `agreement()` and the live generation's `Rehearsed`. Then

- human agreement present and not trustworthy → suspend (unchanged);
- neither present → **return**, `tracing::warn!("no evidence on either side; the base is not moving")`, and Insights says it in words;
- otherwise proceed.

The second bullet is the defect this spec removes. After one lap of probes
exists, rehearsal is always present, so the loop is anchored on every base
that has ever integrated a second capture — which is every base in use.

**The proposal gate.** `propose` picks a candidate by `recommend` over
observation pairs. Before adoption, `rehearsed_under(candidate)` against
`rehearsed_under(live)` on the same probes: a candidate whose MRR falls by
more than one probe's worth is **refused**, journaled as tried (so
`tried_candidates` does not offer it again), and the next pass moves on.
Refused is a new `generations.state` beside `reverted`; both mean "not
offered again".

**The watch.** `holds_up` and `settled` each gain a rehearsal term:

- `holds_up`: false if the lived rule says so (unchanged) **or** the new
  generation's rehearsed MRR is below the parent's by more than one probe's
  worth.
- `settled`: true if the lived rule says so (unchanged) **or** the two
  rehearsed records are within one probe's worth of each other over at least
  as many probes as observations would have needed. This is what ends a watch
  on a quiet base: today `settled` returns `false` for ever where nothing is
  observed, so a quiet base adopts once and is under watch for life.

### 3.3 What this does not do

It does not feed probes into the verdict-paid sweep or into `recommend` as
pairs. `feed_sweep` keeps its meaning; a probe is never a pair in the sense
`candidates` scores. The two kinds of evidence stay in two tables and two
numbers, and Insights shows both, labelled. A rehearsed MRR is **not a recall
figure** — it is the ranking's agreement with the corpus's own later
wording, comparable between two generations and meaningless on its own — and
the page says so beside the number.

---

## Part 4 — Reorganise: on what rehearsal shows

All of Part 4 runs only under `evolve.autonomous = "full"` (Part 6), after
the anchor, before the ranking half's own gates — where `retract` runs
today, and under the same claim. The corpus half now has three rules where
it had two.

### 4.1 Interference: forgetting by displacement

For a probe owner `O` with at least two retained results under the live
generation, if one artifact `X` from another corpus stands above `O` in
**every** retained result — not most, every — then `X` has been answering for
`O`. The cosine at embed time did not call them duplicates; behaviour did.

Action: file `artifact_pairs (O, X)` in state `Pending` with reason
`interference` and the rehearsal ids as evidence, through the same path
`relate` files and with the same `classify_pair` rules. Then the existing
machinery: the judge, `losses`, supersede or merge or escalate, the journal
row, the two retract rules, the undo button. Nothing new at the end of the
chain; the chain gets a new head.

Guards: `action_was_undone` on `(O, X)` in either order → not filed
again, the way dedupe reads the journal. A pair already open → not filed
twice. Neither `O` nor `X` tagged `novel` in `integrations` with no
retained result yet → wait; a thing that has not been rehearsed has not
had the chance to be found.

This is the competition-driven forgetting the evaluation found missing, and
it needs no new retirement path. The handoff's "there is no `stale` kind"
stands: the base still hides nothing on a score. It asks the judge a
question it could not previously ask.

### 4.2 Condense: a version, not a rewrite

An artifact becomes a candidate for a new version when three things hold at
once, each already measured:

1. It is model-written (`captured`, `synthesized`, `merged`). A passage is
   corpus text and is not condensed; a passage that earns it is promoted,
   which exists.
2. It is found — rank within `LIMIT` — in every retained result of at least
   two probes, and the same competitor trails or leads it in every one.
   Stable, and carrying weight it does not need.
3. `engagement_at(O) >= promote.activation_above`. Use has vouched for it;
   the threshold is `promote`'s, not a new one.

Action: arm `Stage::Condense` for `O` — one model call, budgeted by the
queue and by Part 6's weekly cap. The prompt asks for the same artifact
shorter, keeping every value, command, path and flag; the reply is checked
by `losses(&[O], &draft)` with `O` as the single root, and a draft that
loses anything is refused and the unit closes without writing. Condensation
may cost prose. It may never cost a literal, and that is the one line on
which this differs from biological gist, on purpose.

The write, in one transaction:

```sql
CREATE TABLE IF NOT EXISTS artifact_versions (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  n           INTEGER NOT NULL,
  text        TEXT NOT NULL,
  title       TEXT,
  caveats     TEXT NOT NULL DEFAULT '[]',
  created_at  INTEGER NOT NULL,
  -- The corpus_actions row that retired this version.
  action_id   TEXT NOT NULL,
  PRIMARY KEY (artifact_id, n)
);
```

- insert the *current* text as version `n` (the next free `n`);
- `UPDATE artifacts SET text, title, caveats, embed_rev = embed_rev + 1, updated_at`;
- insert `corpus_actions (job = 'sleep', kind = 'condense', subject_id = O, survivor_id = O, evidence = rehearsal ids, detail = version n)`.

Same transaction, the way `set_superseded_by_with` carries its journal row
since the 3b corrections: nothing may be able to read a condensed artifact
with no row. The `embed_rev` bump arms the re-embed the way an edit does; the
probes for `O` are unchanged — they were the question, and the question has
not changed.

Undo, `Core::uncondense(action_id)`: copy version `n` back into `artifacts`,
bump `embed_rev`, stamp the row `undone_at` / `undone_by`. One method, called
by the button and by the base.

**Rule 1 generalises.** `retract::rule_one` asks whether what a merge or a
supersession hid is still found through its survivor. For `condense`,
subject and survivor are the same id and the question is the same: are `O`'s
probes still finding `O` where they found it before the action? Read
`rehearsal_results` for `O`'s probes before and after `at`; if the record
after clears the gate against the record before in the wrong direction, the
version comes back. The same `recommend` shape, pointed the other way, as
rule 1 already is.

A version is a `provenance`-neutral fact: the artifact stays `captured` or
`synthesized`. The detail pane shows "condensed on <date>, version 2 of 2,
restore" and lists earlier versions readable in place.

### 4.3 What salience does in this stage

`integrations.tag` is consumed by exactly two rules and one page:

- 4.1 and 4.2 wait for a `novel` artifact to have been rehearsed at least
  once before acting on or against it.
- Insights shows the tag on every artifact it lists and counts the three
  under "Last night".

It does not enter ranking. Priming on salience is a ranking change, and the
harness rule applies; it is the first candidate for a stage after this one,
with the evaluation harness run on either side.

---

## Part 5 — Wake: the journal

Every pass writes one row, whatever it did:

```sql
CREATE TABLE IF NOT EXISTS sleep_runs (
  id             TEXT PRIMARY KEY,
  started        INTEGER NOT NULL,
  ended          INTEGER NOT NULL,
  -- Why it stopped: finished | activity | suspended | no_evidence | budget
  stopped        TEXT NOT NULL,
  generation_id  TEXT NOT NULL,
  -- Counts, flat, so jobs::did_work reads them.
  integrated     INTEGER NOT NULL DEFAULT 0,
  novel          INTEGER NOT NULL DEFAULT 0,
  known          INTEGER NOT NULL DEFAULT 0,
  conflicts      INTEGER NOT NULL DEFAULT 0,
  rehearsed      INTEGER NOT NULL DEFAULT 0,
  found          INTEGER NOT NULL DEFAULT 0,
  adopted        TEXT,
  reverted       TEXT,
  refused        TEXT,
  undone         INTEGER NOT NULL DEFAULT 0,
  restored       INTEGER NOT NULL DEFAULT 0,
  interference   INTEGER NOT NULL DEFAULT 0,
  condensed      INTEGER NOT NULL DEFAULT 0,
  budget_used    INTEGER NOT NULL DEFAULT 0,
  budget         INTEGER NOT NULL DEFAULT 0,
  -- JSON: the corpus_actions ids and artifact_pairs ids this pass wrote.
  detail         TEXT NOT NULL DEFAULT '{}'
);
```

Insights renders the last seven under **Last night**, in words, one
sentence per phase, every count a link to what it counts — the action and
its undo, the pair and its queue entry, the artifact and its tag:

> Integrated 12 captures — 3 new, 8 known, 1 conflict waiting for you.
> Rehearsed 340 probes; the live generation held (0.61 against 0.60 for its
> predecessor). Filed 1 pair for interference. Condensed 2 artifacts. Took
> nothing back. 3 of 10 actions used this week.

And the states that are not counts, first when they apply: *suspended —
observations no longer agree with verdicts*; *nothing moved — no evidence on
either side*; *stopped — you came back*; *budget spent — 10 of 10*.

This page is what makes "on" defensible: not the guards, which are
invisible, but a morning read of what the base did and a button beside each
line. The `_evolve.html` block keeps its content and becomes the second
section under this one.

---

## Part 6 — Governance: what carries a default of "on"

### 6.1 Three stages

`evolve.autonomous` becomes a string, with `bool` still accepted from an
existing file: `false → "off"`, `true → "full"` (the meaning it had).

| Stage | What the base may do on its own |
|---|---|
| `"off"` | Nothing. The tune pass does not run. Integration still tags and writes probes — that is bookkeeping, and it is what lets the page show *unrehearsed* — but nothing is replayed. |
| `"ranking"` — **the default** | Integrate, rehearse, watch. Adopt and revert ranking generations under the gates of Part 3. No corpus action of any kind: no retract, no interference filing, no condense. |
| `"full"` | Everything in `"ranking"`, plus the corpus half: retract's rules, interference, condense, under the weekly budget. |

The existing dedupe and reap sweeps keep their own switches and consent
blocks unchanged. `"full"` does not turn them on; `"off"` does not turn them
off. They were autonomous before this stage and stay under the switches they
have.

Why `"ranking"` can be the default where `true` could not: every move it
permits is a row in `generations` that `revert_generation` undoes exactly, it
is gated on observations that use produced, it is refused where rehearsal
says it costs, it is watched and taken back where either evidence says it
did not hold, and — the point of Part 3 — it can no longer proceed where
there is nothing to measure it against.

The change of default is its own last task in the plan, after the rest is
built, and it is made only after `cargo test --test eval` has been run on a
real base under `"off"` and `"ranking"` and the two reports read side by
side. The config comment says this is the rule; the rule is kept.

### 6.2 The weekly budget

```toml
[evolve]
# Corpus actions the base may take on its own in any seven days, under
# "full": merges, supersessions, discards, burials and condensations it
# journals. Undos are never counted — taking something back is not spending.
# Reached, the corpus jobs keep finding and stop acting: pairs stay queued,
# nominees stay nominated, and the journal says "budget spent" until the
# window moves. Small on purpose. The worst week the base can have is this
# many actions, each with an undo beside it.
max_actions_per_week = 10
```

Read as `COUNT(*) FROM corpus_actions WHERE at > now - 7d`. Only the base's
own actions are journaled, and an action counts whether or not it was since
taken back, by whom: it was taken, and the budget is a bound on taking.
Checked before every action in dedupe, reap and condense; shown on Insights
as *n of N this week*.

This is a cap on the blast radius, not a rate limit on finding. A base that
finds forty duplicates in a week files forty pairs and acts on ten; the
person sees thirty waiting and can raise the number or press the buttons.

---

## Part 7 — Disclosure

Beyond the journal (Part 5):

- The artifact detail pane: the integration tag with the nearest artifact it
  was read against; the list of probes for this artifact, each a link to the
  capture that wrote it, with the last rank under the live generation; the
  version history where there is one.
- Insights, beside the evolve block: **unrehearsed** — count and list, with
  the sentence *nothing has asked for these*.
- Insights, under the live generation: the rehearsed MRR beside the lived
  record, each labelled for what it is, and the sentence that the first is a
  comparison and not a score.
- `--print-config` names the autonomy stage the way it names `learn.mode`.
- The README's second rule carries its restated wording, and its list of
  things decided against gains cluster schemas and drives with one line each.

---

## Part 8 — Testing

The in-memory vector backend ignores the sparse vector (`memory.rs:310`) and
recency entirely. Every rank this spec measures is a rank under real hybrid
ranking, so the fidelity of Parts 2–4 cannot be asserted against that
backend. Two layers, therefore:

**Arithmetic against a stub.** A `VectorStore` test double that returns
scripted ranks per `(query_vec, params)`. Against it, every rule in this
spec is a unit test named for the rule, in the house style:

- integration tags each of the three ways and never re-tags;
- same-corpus hits are discarded and write no probe;
- a cue probe is minted once per `(artifact, cue)` and never for a passage;
- the lap wraps and the fragile half is walked first;
- a probe under another embedder is retired, not replayed;
- `Rehearsed` noise is one probe's worth and a thin record separates nothing;
- the pass returns on no evidence on either side, and says so;
- a candidate that loses rehearsed MRR is refused and not offered again;
- a watched generation that loses rehearsed MRR is reverted with no
  observations at all;
- a watch settles on rehearsal alone;
- interference needs every retained result and at least two;
- a condensation that would drop a fact token is refused without writing;
- a condensation writes the version, the text and the journal row in one
  transaction, or none of them;
- `uncondense` restores byte-for-byte and stamps the row;
- rule 1 takes a condensation back when the probes stop finding it;
- the budget counts an action that was since undone, by a person or on
  evidence, and stops the next action at the cap;
- `"ranking"` runs no corpus rule, `"off"` replays nothing, `true` means
  `"full"`.

**Fidelity against Qdrant.** The 57 `#[ignore]`d integration tests gain one
per phase: an integration over a two-corpus fixture produces the expected
tags; a probe replayed under two `RankingParams` that differ only in
`recency_weight` produces two ranks the reconstructed recency term explains;
an interference pair is filed for a fixture built to produce one. These are
the contract the stub is written against, and they are run before the
default flips.

---

## What does not move in this stage

- No ranking default. Salience is tagged and shown; nothing reads it on the
  query path.
- No probe is minted from an artifact's own title, body or tags.
- No cluster schema, no drive, no self-posed query.
- No new retirement path: the base hides nothing on a score, and `reap`'s
  nomination rule is unchanged.
- `stale_after_days`, `activation_above`, `min_age_days` stay where the
  stage 3 spec left them; rehearsal gives `activation_above` a candidate
  short signal (probes found vs. missed after a promotion) that a later stage
  can define.
- The verdict-paid sweep and `feed_sweep` keep their meaning.

## Schema summary

New tables: `integrations`, `rehearsals`, `rehearsal_results`,
`artifact_versions`, `sleep_runs`. New `generations.state` value `refused`
(a string column; no DDL). New `corpus_actions.kind` value `condense` and
`job` value `sleep` (string columns). New `Stage` values `Probe`, `Condense`.
New meta keys `sleep.integrated_after`, `sleep.rehearsed_after`. An
interference pair is an ordinary `artifact_pairs` row with its reason and
rehearsal ids in the existing `detail` column. No column is added to any
existing table.

## Order of work

One plan, in this order, each step leaving the tree green:

1. Config: `autonomous` as a three-stage string with `bool` compatibility;
   `max_actions_per_week`. Nothing reads the new stage yet.
2. `rehearsals`, `Stage::Probe`, cue minting at `mark_indexed`.
3. `integrations` and `sleep::integrate` — tags, conflict pairs, capture
   probes. Runs inside the tune pass under `"ranking"` and `"full"`; under
   `"off"` it is called from the retention unit directly.
4. `rehearsal_results` and `sleep::rehearse` — fragile half, lap, cursor,
   retirement of stale probes, expiry with observations.
5. `Rehearsed`, `rehearsed_under`, and the three changes to `tune::pass`:
   the anchor that returns on no evidence, the refused candidate, the watch
   terms. Insights shows both records.
6. `sleep_runs` and the **Last night** section; unrehearsed on Insights; the
   tag on the detail pane.
7. Interference filing, under `"full"` and the budget.
8. `artifact_versions`, `Stage::Condense`, `uncondense`, rule 1 generalised,
   version history on the pane, under `"full"` and the budget.
9. The budget's checks in dedupe and reap.
10. README: rule two restated, two refusals added; `config.example.toml`
    prose for every new key.
11. The Qdrant fidelity tests.
12. Run `cargo test --test eval` under `"off"` and `"ranking"` on a real
    base, read both, and flip the default.
