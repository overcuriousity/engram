# Pairwise merging, and the end of the fan-in cap

Date: 2026-08-17
Status: approved, ready for an implementation plan

## The problem

Sixteen pairs sit in `Oversized`, every one of them with `judge_attempts = 0`.
The model has never been asked about any of them and, as the code stands, never
will be.

The mechanism is short. `dedupe::run` expands its seed pair into the connected
component of open pairs around it, flattens that component to its captured roots
(`src/jobs/dedupe.rs:146`), and refuses outright when the flattened set exceeds
`consolidate.merge_max_roots` (`src/jobs/dedupe.rs:150`). The refusal settles
every pair in the component as `Oversized` before any call is made.
`pairs_to_judge` selects `WHERE state = 'pending'`
(`src/store/pairs.rs:437`), so `Oversized` is terminal: nothing retries it, and
no operator action short of direct SQL reopens it.

Two components account for the sixteen rows: one of twelve roots across nine
pairs, one of nine roots across seven. The cap is eight, which nobody set — it
is the default from `config.example.toml:260`, and `config.toml` does not
mention it. Merging was quietly off for any cluster past eight sources.

The size of those numbers is the important part. Twelve artifacts of a few
hundred tokens each is nowhere near any context ceiling. These components were
not refused because they could not be judged; they were refused because a
counter said eight.

## What is actually wrong

Two things, and only the second one is about size.

The unit insists on answering an entire component in a single call. That is what
makes fan-in a property the unit has to defend against at all: a twelve-root
component only ever needed a twelve-root prompt because one call had to settle
all nine of its pairs at once.

A pair can reach a terminal state without ever having been asked about.
`Dismissed` earns that right, because structure answers it — a member is gone,
or the two members flatten to a single root, and there is genuinely no question
left to put to a model. `Oversized` is answered by nothing. It is the system
declining to do its job and recording the decision as done.

## Goals

Merge two artifacts at a time, and let the results be merged again. Delete
`Oversized` as a state rather than re-triggering it on a better measurement.
Reopen the sixteen rows already written. Deprecate `consolidate.merge_max_roots`.

Non-goals: changing how pairs are found and filed (`relate`, `associate`,
`describe` are untouched), changing the four verdicts, and changing the loss
check's two rules. This is about which artifacts reach the judge and what
happens after it answers.

## The design

### 1. The unit is one pair

`dedupe::run` stops calling `open_component`. It reads the seed pair, resolves
its two members, and asks about exactly those two.

`open_component` stays in the store — the sweep and the web UI still use it to
reason about clusters — but it leaves the dedupe path entirely.

The retired-member handling collapses to two lines. If either member is not
`in_results`, the pair is dismissed and the unit returns. The partition dance at
`src/jobs/dedupe.rs:91-113`, which separates pairs naming a retired member from
their still-live siblings, exists only because one unit owned many pairs. It goes
away with them, and so does the class of bug it was written to fix.

Two structural guards survive, restated for two members:

A member that is `Merged` and has no rows in `artifact_sources` is a merge whose
sources were deleted out from under it. Its text is a paraphrase with nothing
behind it. Settled `Contradiction`, as today, with the same detail.

If one member appears among the other's captured roots, the pair is dismissed.
This is the pairwise form of today's `root_ids.len() < 2` guard: a merge and one
of its own sources are not two things to compare, and asking would spend a call
to be told an artifact matches itself.

### 2. The prompt, and the end of the size gate

Each of the two members contributes its own text under a letter, `A` and `B`.

A member that is `Merged` additionally contributes its captured roots as a
labeled context block — reference material the model may draw detail back from,
and explicitly not a merge input. Context roots carry no letters and cannot be
named by a verdict.

`dedupe_prompt` changes shape accordingly. Today it takes a slice of
`(title, text)` and letters them all (`src/infer/prompt.rs:238`). It should take
the two members and, separately, the context blocks, so that lettering cannot
reach the context by construction.

`DEDUPE_SYSTEM` gains one paragraph: some artifacts are shown with the original
captures they were merged from; those are there so that detail lost in an earlier
merge can be restored; they are never named in `supersedes`.

**There is no size gate.** The merge inputs are two artifact texts, bounded by
what capture already bounds. What can grow without limit is the context block,
and context is reference rather than input, so it is trimmed rather than
defended against: assemble the prompt, and while `checked_ceiling_for_prompt`
(`src/infer/budget.rs`) reports no room for a reply worth having, drop the oldest
context root and re-measure. Trimming degrades an answer. It never blocks one.

If the two member texts alone still do not fit, that is a different failure with
a different cause — an artifact large enough that no pair containing it can be
judged — and it is settled `Contradiction` with a detail saying so. That is an
escalation to a person about a real condition, not a counter firing.

`PairState::Oversized` is removed from `PAIR_STATES` (`src/web/ui.rs:1077`) and
from everything that writes it. The enum variant and its `"oversized"` string
mapping stay for one release so that existing rows still parse.

### 3. Letters index members, which removes a live bug

Today `supersedes` is resolved against `roots` while the members are a different
list, and `src/jobs/dedupe.rs:261-266` documents what that cost: whenever a
component contained an earlier merge the two lists diverged, and a letter
resolved against the wrong one superseded an artifact the model had never been
shown.

With two lettered members and unlettered context, the two lists cannot diverge.
The newest-wins check in `interpret` now compares the two members' `created_at`
directly, and `apply`'s `Replaced` arm supersedes the named member in favour of
the other one.

The `survivors` branch of that arm — the pairs whose both sides survived a
supersession, settled `Contradiction` with "these two were not separated" — has
nothing left to describe once a unit owns a single pair, and is deleted.

### 4. The loss check runs against what was merged

`losses(&roots, draft)` currently compares the draft against every captured root
(`src/jobs/dedupe.rs:289`). Kept as written under repeated pairwise merging, a
fact dropped in the first generation would fail every later merge in that
lineage forever, and the lineage would freeze.

So the draft is checked against **what was actually merged**: the two member
texts. First-generation merges are unchanged by this, because their members are
their roots. Loss stays one generation deep per merge step, which is the property
the flattening was protecting, and the context block is what gives the model the
chance to reverse earlier drift instead of compounding it.

This is the one place where the new design is weaker than the old one, and it is
deliberate. The old design bought "never more than one generation from captured
wording, however many merges" at the price of never merging past eight sources at
all. The new one trades a bounded, model-visible drift for a system that
converges.

### 5. Siblings are re-pointed onto the merge

When A and B merge into M, the pending pairs naming A or B are answered by
neither the merge nor the old member. Left alone they die with their member
(`src/jobs/dedupe.rs:91-101`) and wait for M to embed and a later sweep to re-file
them against C — correct, but three artifacts then take three ticks.

Instead, a new store method rewrites them:

```rust
pub async fn repoint_open_pairs(&self, old: &[String], new_id: &str) -> Result<u64>
```

It updates `artifact_pairs` rows in state `pending` whose `a_id` or `b_id` is in
`old`, replacing that side with `new_id`, subject to three rules:

- A row whose other side is already `new_id` would become a self-pair. Dismissed,
  not updated.
- A row that would collide with an existing pair between the same two artifacts
  is dismissed rather than updated, in any state the existing row is in. This is
  what keeps an operator's earlier `Dismissed` respected forever, the same
  property `record_pair`'s `INSERT OR IGNORE` provides.
- `judge_attempts` and `judge_unreadable` reset to zero. The re-pointed pair asks
  a different question than the one that accumulated those counts, and inheriting
  a backoff earned by a different pair of artifacts would punish a fresh
  question. This cannot spin: every merge reduces the number of active artifacts
  by one, so the sequence terminates.

The `score` is left as it was and is now stale — it was measured between C and B,
not C and M. It is a queue ordering hint, not a threshold at this stage, so the
staleness costs ordering accuracy and nothing else.

**Timing: this happens in `merge::finish`, not in `merge::write`.** At `finish`
the merge is indexed and its sources are superseded. Re-pointing at write time
would arm a unit that could merge M into M2 before M had ever superseded A and B,
leaving both of them active underneath a deprecated chain.

### 6. Reopening what is already written

Every `Oversized` pair goes back to `Pending` with its detail cleared, in the
consolidate sweep, on every tick. Once nothing writes the state, the first tick
drains it and every later one is a no-op against an empty set — so this needs no
migration machinery and no run-once guard.

It is safe precisely because of what the diagnosis found: `judge_attempts = 0` on
all sixteen rows means no work is being redone and no backoff is being reset.
They enter the queue at the front, since `pairs_to_judge` orders
`judge_attempts ASC` before score.

The twelve-root component then settles as a sequence of pairwise calls spread
across ticks, each one merging two artifacts and re-pointing the rest onto the
result, rather than as one call that is refused.

### 7. Configuration

`consolidate.merge_max_roots` becomes `Option<usize>` in `ConsolidateConfig`,
unread by anything, and logs a deprecation warning at load when a config file
sets it. The clamp at `src/config.rs:841` and its test at `src/config.rs:1136`
are deleted — that clamp existed to stop a value below two from settling every
component `Oversized` with merging silently off, which is exactly what the
default of eight was doing at a less obvious threshold.

The key is removed from `config.example.toml:260` and from the `consolidate.*`
row in `README.md:185`. The doc comments that reference it as a bound on prompt
size (`src/infer/budget.rs:74`, `src/infer/openai.rs:829`,
`src/infer/openai.rs:1655`, `src/store/pairs.rs:64`) are rewritten to describe
the budget-trimmed context block, since those comments are the record of why the
budget code is shaped the way it is and would otherwise point at a key that no
longer exists.

## What this costs

Four duplicates settle in three calls across three embed-and-finish cycles,
where today they settle in one call on one tick — when the component is under the
cap. Over the cap, today they do not settle at all.

The module header at `src/jobs/dedupe.rs:10-14` argues against exactly this, on
the grounds that pairwise settlement writes intermediate merges that are
superseded almost immediately and thrown away for nothing. That argument assumed
the intermediates are waste. With transitive root inheritance — which
`insert_merged_artifact` already provides, resolving `roots_of(sources)` into the
flattened closure (`src/store/artifacts.rs:238-249`) — and with siblings
re-pointed onto the result, each intermediate is a real artifact that the next
step builds on. The header is rewritten as part of this work; it is the primary
statement of the unit's design and leaving it contradicting the code is not an
option.

## Testing

Behaviour tests, at the level the existing dedupe tests work at:

- A merge of a merge: A and B merge into M, M and C merge into M2, and `M2`'s
  `artifact_sources` names A, B and C.
- A twelve-root cluster converges to a single artifact across successive ticks,
  and no pair is ever settled `Oversized`.
- A `Merged` member's roots appear in the prompt as context and its own text
  appears as a lettered input; a `supersedes` letter never resolves to a context
  root.
- Context roots are trimmed oldest-first when the budget is tight, and the two
  member texts survive the trim.
- Two member texts that alone exceed the window settle `Contradiction`, not
  `Oversized`.
- Re-pointing: a pending sibling is rewritten onto the merge at `finish`; one
  that would become a self-pair is dismissed; one that would collide with an
  existing dismissed pair is dismissed and the existing row is untouched;
  `judge_attempts` resets.
- Re-pointing does not happen at `write` — a merge whose embed never lands leaves
  siblings pending against the original members, which `reap_stranded` already
  covers.
- The loss check fails a draft that drops a value present in a member's own text,
  and passes one that drops a value present only in a context root.
- An existing `Oversized` row is reopened by the sweep with attempts intact at
  zero, and a second sweep reopens nothing.
- A config file setting `merge_max_roots` loads, warns, and changes no behaviour.

## Risks

Drift compounds one generation per merge step rather than staying one generation
from capture. The context block is the mitigation and it is a soft one: the model
may or may not use it. Worth watching on the twelve-root component once it
converges, by reading the final artifact against the twelve originals.

Convergence is slower and more visible. A cluster resolves over several ticks
with intermediate merged artifacts appearing in search in between. This is
correct — an intermediate is a better artifact than the two it replaced — but it
is a behaviour change an operator watching the base will notice.

Re-pointing writes to pairs from `merge::finish`, which runs from `mark_indexed`
and from the sweep's recovery path. Both need to be idempotent against a pair
already re-pointed by the other; the collision rule provides that, but it is the
part of this design most worth reviewing carefully.
