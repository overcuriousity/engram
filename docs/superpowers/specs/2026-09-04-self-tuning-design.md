# A base that tunes itself, and earns its way out of the loop

## Why

Everything about how an artifact is *found* is a parameter, and every one of
them was set once by hand. `[recommend]`, `[associate]`, `[activation]`,
`[promote]`, `[sitting]`, the fusion weights, the per-source cap — a number
chosen on a base of five hundred artifacts, still standing at fifty thousand.
Issue #78 names four shipped defaults waiting on a harness run that nobody has
time to make routine.

One loop already closes: a verdict on the judge bar buys a sweep
(`src/eval/sweep.rs`), the sweep produces a recommendation, and someone with
the judge grant presses apply, which writes two keys into `config.toml`. It
works, and it is limited in exactly three ways. It moves two knobs of the
dozens that matter. It runs only on human verdicts, which a personal base
produces a handful of a week. And a person has to be there.

This design widens all three. The parameters that decide **what is retrieved
and in what order** become a named, versioned thing the system moves on its own
evidence and reverts on its own evidence. The evidence comes from use rather
than from a verdict bar, at roughly two orders of magnitude more volume. And
the jobs that touch the corpus itself — merge, promote, reap — stay behind a
human press until they have earned their way out of it, measured the same way
as everything else.

The cargo harness (`tests/eval.rs`) is untouched and stays the instrument of
long-term comparability. This design is what makes its numbers attributable:
every ranking change becomes a named generation, so a score that moved has
something to have moved *because of*.

## The three decisions this rests on

Written down because the mechanism below is only correct under them, and a
later reader will otherwise reopen them one at a time.

**1. Autonomy is full, with rollback.** The system changes itself and reverts
itself. It does not queue changes for approval. The price is that "why did this
number move" must be answerable at any moment, which is what Part 1 exists for.

**2. Autonomy covers access, not the trace.** Anything that decides how a thing
is found may move on its own. Anything that rewrites, merges or retires stored
content proposes, and earns the right to act only by being right (Part 5). The
second of the three rules in the README holds throughout, but it holds for a
reason worth naming: it rests on the corpus jobs' own guards — originals kept,
nothing lost on a score, undo on everything — not on a human being present.
Where that guard is weak, autonomy does not follow; see reap.

**3. Evidence from use is admissible as the primary signal; human verdicts
become the anchor.** What the system optimises is *the generator and the
operator could use what retrieval gave them*, which is one step removed from
*the person got their answer*. Verdicts are the sparse, honest sample that
checks the two have not come apart.

Drift across many small steps is accepted. The corpus is untouched by any of
this, so the worst case is a ranking that has wandered somewhere nobody chose —
recoverable, because the operator's starting point is never overwritten.

## Part 1 — Generations and the journal

A **generation** is an immutable, numbered row holding:

- the ranking parameters (the reordering knobs — `recency_weight`,
  `per_source_cap`, fusion weights)
- the retrieval parameters (the knobs that change what is fetched at all —
  candidate pool depth, rerank on/off, `associate.prime_lift`, spread)
- **the identity of who computed under it**: the embedding recipe fingerprint
  that `infer.embed` already builds from model, dim and the three templates,
  and the synthesize/ask model name
- its provenance: the parent generation, the idle run that proposed it, and the
  improvement it predicted
- its state: `proposed`, `live`, `reverted`, `superseded`

Exactly one generation is live per tenant. Serving is deterministic: the same
query in two sittings returns the same order, which is the property
`[sitting] prime = false` exists to protect and which no part of this design
may spend.

Pinning the model identity is not bookkeeping. Change the ask model and every
citation-derived number shifts underneath the evidence. A generation that does
not name its models is a row of numbers nobody can compare to another.

### The live generation lives in the database, not in `config.toml`

Today `write_ranking` (`src/config.rs:1925`) edits the operator's file through
`toml_edit` specifically so that "a serialize-and-overwrite would return the
operator's commented file as a machine's" — the reasoning is in `Cargo.toml`. A
system that rewrites that file every idle period defeats it by volume rather
than by carelessness.

So the split is:

| Holds | What it says |
|---|---|
| `config.toml` | The starting point, and the envelope: which parameters may move and between what bounds. Never written by the autonomous loop. |
| The tenant database | The live generation, and every generation before it. |

`--print-config` must report both — the file's values and the live generation's
— or an operator reads a file that no longer describes what is running. It
already prints `learn.mode` first and then the keys the mode decided; this is
the same disclosure, one layer out.

Manual return to the starting point is therefore always available and always
cheap: it is one row becoming live. This is the whole of the recovery story,
and it is enough because nothing in the corpus was ever at risk.

### The journal

Every adoption and every revert is recorded with its reason, its evidence
count, its prediction, and what actually happened. `src/store/eval_runs.rs`
already stores runs with `base_params`, `best_params`, both metrics, a
per-query `DiffRow` and whether it was applied. It widens to carry generations
and outcomes; it is not replaced.

## Part 2 — Evidence: what use leaves behind

Every retrieval attempt that terminates in something produces one or more
**observations**: the query, its stored vector, the artifact the observation is
about, a strength, its source, and **the generation it was collected under**.

That last field is what makes the whole design work. An observation collected
under generation 7 is a statement about the list generation 7 produced, so
re-scoring a candidate against it is a counterfactual — asking where *this*
configuration would have placed the artifact that turned out to matter, without
ever having served it.

### Sources, all present in the tree today

| Source | Where it already is | Strength |
|---|---|---|
| An excerpt was used to build an answer | `NewAskCitation.used`, from `check::referenced` | strong positive, at that excerpt's rank |
| A result was opened, expanded, `--show`n | `opened_at` on the search event; `record_interaction` | strong positive, at its recorded rank |
| An answer asserted a literal no excerpt supports | `check::unsupported_literals` — "No inference: this is a string operation over text already generated" | negative: retrieval failed to supply what the answer needed |
| A search nobody opened, followed by another search from the same scope within a few minutes | derived from `created_at`, `scope`, `opened_at` | **weak** negative |
| Nothing happened | — | **not an observation.** Silence is scored as nothing, in either direction |

The last two rows are the load-bearing ones and the reason for the weighting.

`fold_onto` is **not** a reformulation signal and must not be read as one. It
coalesces a typing burst into a single event and overwrites the intermediate
wordings on purpose: "what survives is the final wording, the query that was
actually meant." Its guards are useful here for a different reason — a judged
or opened event is never folded into, and `coalesce_secs` defaults to 5 — which
means consecutive stored events are genuinely distinct search acts rather than
keystrokes. The give-up chain is derived from those distinct events, not read
off `fold_onto`.

### The asymmetry

**A weak negative may never cause an adoption. It may only cause a revert.**

The search door has a success case indistinguishable from failure: the rail
shows snippets, and someone who reads their answer straight off the list and
walks away looks exactly like someone who gave up. `config.example.toml`
already names this case as what the unmatched-gap sweep exists to catch. Ask
does not have the problem — it says which excerpt was used.

So search evidence is structurally weaker than ask evidence, and the design
treats it that way rather than pretending otherwise. Weaker evidence is enough
to stop something, never enough to start it.

### Scoring

Not "what fraction of what was shown got used". That ratio is maximised by
showing less, and a system optimising it converges on retrieving one thing.

Instead: **the position of the observed artifact within the list, at fixed
depth.** The same quantity the existing evaluation reports, computed against
self-generated observations instead of hand-given ones. Shrinking the pool buys
nothing.

### Verdicts as anchor

Human verdicts continue exactly as they are and are never used for volume. They
form a standing check that the self-generated score still moves in the same
direction as human judgement. If that agreement decays past a bound, **the
system suspends its own autonomy and says so on Ops.** This is the one
safeguard the rest of the design leans on; everything else is recoverable by a
revert, and this is the thing that notices the score itself has gone bad.

## Part 3 — The idle pass

Triggered after a configured quiet period (default 30 minutes). "Cold" is
already a concept — `Carried::is_cold`, `Sittings::is_empty`, `Working::is_idle`
— and the retention ticker already exists to hang background work on. Nothing
new is invented for the trigger.

**Which observations are replayed.** Not all of them, and not uniformly:
prioritised by how wrong the system was. Abstentions first, then give-up
chains, then answers carrying unsupported literals, then observations whose
artifact sat far down the list. Replaying what surprised the system is both the
biologically faithful choice and the one that keeps the pass bounded.

**Which candidates are tried.** Not an exhaustive grid — the existing sweep's
5 × 4 over two axes does not survive widening to the retrieval knobs.
Candidates are drawn preferentially toward configurations that have looked good
in past runs, with deliberate exploration alongside. The exploration lives
here, in the idle pass, and never on the query path.

**What it costs.**

- Reordering candidates re-sort the **stored** candidate list from
  `search_candidates`. No retrieval, no embedding. This is what `sweep.rs`
  already does and why it is seconds of work.
- Retrieval candidates must genuinely re-search, because they change what is
  fetched. Stored `query_vec` means no embedding call; the cost is vector reads,
  taken while nobody is waiting.

**The pass makes no model calls at all.** It is neither write time nor read
time and spends no inference. It takes vector-store permits from a single
bounded pool, the way `jobs::gaps` already does with `COVER_IN_FLIGHT`, so an
idle pass cannot become the load the whole memory shares.

It must be interruptible the moment someone returns, and resumable on the next
quiet period. That pattern is established in the tree.

**The common and correct outcome is "nothing beat the live generation."**

## Part 4 — Adoption, watch and revert

### Adoption

1. **Positive observations only.** Weak negatives cannot promote a candidate.
2. **A floor on evidence.** Below it, nothing moves. `docs/evaluation.md`
   already sets the honest version of this for verdicts: under twenty, "the
   arithmetic works and the result means nothing."
3. **One parameter per adoption.** Move three and lose the ability to say which
   one did it. Slower, and it keeps the journal readable and the revert exact.
4. **The prediction is recorded** — how much this generation should improve the
   position of observed artifacts.

### Watch

The new generation serves. Fresh observations accrue under it and are compared
against the prediction, and against the **lived** record of its predecessor —
never against the predecessor's offline number, which was computed on replayed
evidence and is not the same kind of quantity.

The watch window is measured **in observations, not in days**. A base in heavy
use decides quickly; a quiet one waits. This self-paces and is more honest than
any fixed period.

### Revert

Miss the prediction, or come in worse than the predecessor, and the system
switches back on its own and journals the failed adoption — **including the
memory of it**, so the same candidate is not proposed again on the next quiet
period. Without that memory the system oscillates.

A reverted candidate is not banned forever. It becomes eligible again when the
ground under it has changed: the corpus has grown substantially, or the models
have changed.

Reverting is cheap and complete because a generation is a row. Nothing in the
corpus was touched. This is precisely why the retrieval and ranking parameters
get autonomy and the corpus jobs do not — yet.

## Part 5 — Earned autonomy over the corpus jobs

Merge, promote, consolidate, dedupe, reap and judgement start by proposing.
They do not stay there.

### Proposing

A proposal must be judgeable **without performing it**. For thresholds it is:
which pairs would cross a different threshold is computable without merging
anything. So the idle pass computes the dry run and shows it —

> the merge threshold is at X; at Y, fourteen further pairs in the last weeks
> would have merged, and here they are; of those already merged, two were never
> retrieved again

— gated behind the existing `can_judge` grant. No new permission concept.

### Earning

Every proposal records what it proposed. The operator's response — applied,
rejected, or left standing — is itself an observation, and agreement rate per
job and per confidence band is a measurable quantity like any other.

A **band** is not a new concept: `[consolidate]` already has two lanes —
`review_min = 0.88`, the score at which a pair is worth asking about, and
`auto_supersede = 0.95`, the lane asked about first — and a configuration that
leaves no band between them is refused at startup. Autonomy is granted per
lane, and the confident lane graduates first.

When agreement over a sufficient number of decisions passes a threshold, that
job's proposals in that lane begin applying on their own. Autonomy is earned by
demonstrated accuracy, which is the same discipline this whole design applies
to everything else, turned on the system's own judgement.

### Losing it

Autonomy here is not a ratchet. An auto-applied action that the operator later
undoes is a disagreement, it lowers the agreement rate, and the job falls back
to proposing. The undo paths already exist: merges keep their originals,
promotion has its undo, consolidation is reversible by design.

**Reap graduates last, and not on this mechanism alone.** What it does is
"wiped from search and index, full text kept in a graveyard table that nothing
reads back" — an undo that exists as data but not as a path. Reap may not
become autonomous until a restore path exists that a person can actually take.

## When the ground moves

**The models change.** A generation names its embedding recipe and its chat
model. The embedding side is already caught at boot — engram refuses to start
when stored vectors do not match the configured recipe. The chat model is not
caught by anything today; that is a small new check at boot against the live
generation. When either has changed, prior observations are **marked as another
era, not deleted** — nothing is deleted here — and are no longer eligible for
adoption. A new line starts from the live generation's parameters, and Ops says
so rather than the counting silently continuing.

**The corpus grows.** What was right at five hundred artifacts is wrong at
fifty thousand, especially the per-source cap and the pool depth. This is not a
failure mode; it is the strongest argument for the design. The system re-decides
continuously instead of once at setup.

**An observed artifact is merged, superseded or reaped.** The existing rule
applies unchanged: an observation is satisfied by whatever **superseded** the
artifact it names, or merging would report a retrieval regression that is
really a bookkeeping change. An observation naming something that no longer
exists at all is marked unusable and **excluded**, never scored as a miss — a
miss is a claim about ordering, and this is not one.

## Order of work

This is three plans, not one, and the split is where each stage stops being
useful on its own.

**Stage 1 — generations, the journal, and observation collection.** Behaviour
is unchanged: the running configuration is named a generation, and use starts
leaving observations behind. Nothing adopts anything.

Worth shipping alone, and by some distance the best return here: it hands the
*existing* sweep roughly two orders of magnitude more evidence than the judge
bar produces, with a person still pressing apply. Issue #78's four waiting
defaults become answerable in weeks instead of never. If the rest of this
design is never built, stage 1 still pays for itself.

**Stage 2 — the idle pass, adoption, watch and revert.** Autonomy begins, over
the reordering parameters first — where a candidate is scored by re-sorting a
stored list and nothing is retrieved — and over the retrieval parameters once
that loop has a track record in the journal.

**Stage 3 — earned autonomy for the corpus jobs.** Needs stage 2's agreement
history to exist before it can mean anything, and needs a reap restore path
before reap is in it at all.

## Configuration

Under a new section, with the envelope as the operator's half of the contract:

- the quiet period before an idle pass (default 30 minutes)
- the window within which an unopened search followed by another search counts
  as a give-up — minutes, and deliberately far above `coalesce_secs`
- per-parameter bounds: which may move, and between what values
- the evidence floor before anything is adopted
- the watch window, in observations
- the agreement threshold and decision count at which a corpus job earns
  autonomy, and the level at which it loses it
- the anchor-agreement bound below which the system suspends itself
- one switch that turns the whole loop off, leaving today's behaviour exactly

Consistent with `[learn]`: one dial states the intent, every key it stands for
remains a key, and a key written in the file wins.

## Error handling

- An idle pass that fails partway leaves the live generation alone and is
  retried on the next quiet period. A pass is never partially adopted.
- A vector store that is unreachable ends the pass; it does not degrade the
  serving path, which never depended on it.
- A generation naming models that are no longer configured cannot be made live;
  the boot check refuses it and says which side moved.
- Suspension is a state, not a failure: the loop stops adopting, keeps
  collecting, and Ops reports why.

## Testing

- Reordering candidates scored against a stored candidate list, with no vector
  store, using the deterministic fakes the rest of the suite runs on.
- The give-up chain: an unopened search followed by another inside the window
  is a weak negative; the same pair an hour apart is nothing; a search whose
  result was opened is never negative.
- Silence produces no observation, asserted directly.
- A weak negative alone never adopts; the same evidence reverts.
- One adoption moves exactly one parameter.
- A revert restores the predecessor and records the candidate as tried.
- Anchor decay suspends adoption and leaves collection running.
- An observation whose artifact was superseded is satisfied by the successor;
  one whose artifact is gone is excluded rather than scored as a miss.
- A corpus job crosses the agreement threshold and begins applying; an undo of
  an auto-applied action returns it to proposing.

## Out of scope

- **Serving exploration.** No live A/B, no per-request variation. One
  generation is live and the same query ranks the same way twice.
- **Tuning the embedding recipe.** It cannot be swept at runtime; it belongs to
  the cargo harness, which re-embeds a frozen corpus for exactly this reason.
- **Prompt evolution.** The generator's prompts are inside the measurement, not
  subject to it. Moving them would move every number underneath the evidence.
- **Reap autonomy**, until a restore path exists.
- **Cross-tenant learning.** Generations are per tenant, like everything else.
