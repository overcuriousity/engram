# Autonomous consolidation — Design

Date: 2026-08-14
Status: draft
Extends `src/jobs/consolidate.rs`, `src/jobs/judge.rs`, `src/store/pairs.rs`.
Overturns one stated invariant. See §3.

## 1. Why

Consolidation today finds duplicates and then asks a person about them. That is
two different jobs, and both of them scale badly.

**Detection is a lottery.** `near_pairs` draws a random sample of
`consolidate.sample` (2000) points per sweep and computes the distance matrix
only *within* that sample. For a given duplicate pair to be found, both members
must land in the same draw — probability ≈ (s/N)². At `interval_hours = 24`:

| Artifacts | P(pair found per sweep) | Expected wait |
|---|---|---|
| 5,000 | ~16 % | ~6 days |
| 20,000 | ~1 % | ~3 months |
| 100,000 | ~0.04 % | ~7 years |

This decays quadratically. Nothing downstream can fix it: a judge that never
sees a pair cannot rule on it.

**The queue has no consumer that scales.** Pairs that survive the prefilter go
to `artifact_pairs` and wait for an operator. The queue grows with the base; the
operator does not.

**The prefilter has the wrong polarity for deduplication.** `may_disagree`
(`src/infer/facts.rs:103`) admits a pair only when both sides state values *and
those values differ*. That was exactly right for the question it was written
for — "do these two contradict each other?" It is backwards for deduplication.
The pairs it discards are the cleanest merge candidates:

```
"Mount the filesystem before writing to it."   ->  fact_tokens: {}
"Attach the volume before writing."            ->  fact_tokens: {}
   may_disagree = false  ->  NoConflict  ->  both stay active, both in search
```

Two artifacts at ~0.93 similarity saying the same thing are filed as "nothing to
decide" and both remain in every result set. That is the duplication this
subsystem exists to remove, and today it is structurally invisible to it. The
test at `src/infer/facts.rs:122` states the behaviour plainly.

## 2. Goal

Duplicate hygiene runs without an operator, on a base that grows, while every
decision stays reversible and every value stays recoverable.

Concretely:

1. Detection is complete rather than sampled: a pair of near-identical artifacts
   is found once both are indexed, not eventually.
2. A near-duplicate group settles itself — superseded where an original
   suffices, merged into one rewritten artifact where both sides carry something
   the other lacks.
3. A genuine disagreement about a value is **not** settled autonomously. It goes
   to a person, as today.
4. Nothing a merge produces can silently lose a value, a command, or a path.
5. Every merge is undoable, and the undo survives the next sweep.

### Non-goals

- **Deciding which of two contradictory facts is true.** Explicitly out. This
  stays the reader's judgement and keeps its existing escalation path.
- **Merging as the preferred outcome.** Where one original plainly replaces the
  other, superseding is better and is chosen first. See §6.
- **Retrieval-time inference.** Unchanged: a query costs one embedding and one
  vector search. Everything here happens in the background.
- **Removing the review queue.** It shrinks to conflicts and refusals. It does
  not disappear.

## 3. The invariant this overturns, and what replaces it

Four places in the repository state that artifacts are never merged:

- `src/store/schema.sql:51` — "Superseding hides an artifact and names its
  replacement; nothing is ever merged or rewritten in place."
- `src/jobs/consolidate.rs:10` — "A merged artifact would be synthetic text
  standing where a stored passage used to, with no segment to verify it against
  and no corpus lines to show beside it, which is the one failure mode this
  design exists to avoid."
- `ROADMAP.md:23` — "Fidelity outranks convenience… A paraphrase or a synthetic
  summary must never silently replace or outrank the original wording."
- The ROADMAP `CUT` block, which killed precomputed answer cards for exactly
  this reason.

The prohibition is deliberate and load-bearing. It is lifted here **narrowly**,
and the four mechanisms below are what stand in its place. All four are
non-negotiable parts of this design; dropping any one of them turns the feature
back into the thing the prohibition was protecting against.

1. **Synthetic text is the last resort, not the default.** A group where one
   artifact plainly replaces the other is superseded, keeping stored wording
   with a valid span. A merge happens only when both sides carry information the
   other lacks — the case where *neither* original is sufficient and the
   pre-merge state was already losing something.
2. **Provenance is explicit and structural.** `provenance = 'merged'`, and every
   merged artifact carries rows naming the captured artifacts it came from. It
   is never mistakable for a captured passage, in the store or in the UI.
3. **The originals are never destroyed.** They are superseded — hidden, still
   stored, still readable, one write from being back. Same mechanism, same undo
   as today.
4. **The merge is verified against the originals.** No value and no literal may
   disappear (§6.4). A merge that would lose one is refused, not applied.

What remains true and is not weakened: retrieval returns whole artifacts, never
generated prose; a captured artifact is never rewritten in place; nothing is
deleted on a similarity score.

## 4. Data model

### 4.1 Artifacts gain a kind

```sql
-- artifacts
provenance      TEXT NOT NULL DEFAULT 'captured',   -- 'captured' | 'merged'
corpus_id       TEXT NULL REFERENCES corpora(id) ON DELETE CASCADE,  -- was NOT NULL
lifecycle_dirty INTEGER NOT NULL DEFAULT 0
```

`provenance` is the discriminator every consumer branches on — never
`corpus_id IS NULL`. A null is an absence; a kind is an assertion, and the
failure modes this feature can produce want to hang off an assertion.

A merged artifact has `corpus_id`, `corpus_span` and `segment_idx` all NULL. It
belongs to no corpus because it came from more than one, and claiming a span it
does not have is the specific dishonesty §3 exists to prevent.

**Consequence for deployment.** `src/store/schema.sql` is applied on every
connect and cannot alter an existing table (`schema.sql:9–12`); `migrate` reads
the columns back and checks them. Adding `provenance` and `lifecycle_dirty` is
additive and safe. Dropping `NOT NULL` from `corpus_id` is a column *change* and
therefore means **recreating the database**. The schema header names this as the
intended path during testing. Recorded here so it is a decision and not a
surprise.

### 4.2 Lineage, stored as the transitive closure

```sql
CREATE TABLE IF NOT EXISTS artifact_sources (
  child_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  root_id    TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  -- The direct parent through which root_id entered this child. Equal to
  -- root_id for a first-generation merge. Rendering only.
  via_id     TEXT REFERENCES artifacts(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (child_id, root_id)
);
CREATE INDEX IF NOT EXISTS idx_sources_root ON artifact_sources(root_id);
```

Resolved roots, not edges. `root_id` always names an artifact with
`provenance = 'captured'` — this is an invariant with a test (§11).

The denormalisation is what makes re-merge cheap at scale: a candidate group's
roots are one `SELECT ... WHERE child_id IN (...)`, with no recursive CTE on the
sweep's hot path, and the fan-in cap is `COUNT(DISTINCT root_id)` — checkable
*before* a model call is spent.

The redundancy is real and is the price: a deleted root removes closure rows
that an edge table would have let us recompute. §8.3 says what happens then.

### 4.3 What `superseded_by` keeps doing

Nothing new. A merge calls `Core::supersede` per root, with the same guards
(refuses a deprecated side, SQLite first, payload second) and the same Ops undo.
`artifact_sources` is additive: `superseded_by` answers "where do I look
instead", `artifact_sources` answers "what is this made of".

No merge-log table is needed. `superseded_by` + `artifact_sources` +
`artifacts.created_at` reconstruct any merge. An Ops view over that is a query,
not a write path.

### 4.4 Pair states

`PairState` gains two variants:

- `Oversized` — the group exceeds `merge_max_roots`. Not merged, surfaced to
  Ops.
- `NearIdentical` — the pair scored at or above `auto_supersede`. It is settled
  by the sweep's free clustering pass and **never** arms a dedupe unit. See
  §5.2, which is where this variant becomes necessary.

`Superseded` keeps its meaning (a proposed direction, `obsolete_id` set) and
gains an applied form under autonomy. `Contradiction` becomes the sole human
queue.

## 5. Detection

### 5.1 The primitive already exists

`VectorStore::neighbours(artifact_id, limit)` (`src/vector/qdrant.rs:1826`)
queries Qdrant **by point id**, so the vector is looked up in the index and no
embedding call is paid. It already filters `superseded`/`deprecated` correctly
and returns scores. Both backends implement it. Its only caller today is the
detail pane's related list (`src/web/ui.rs:1301`).

No new vector capability is needed — only a new caller.

### 5.2 One unit per artifact, triggered by embedding

```
Stage::Relate, target_kind = "artifact"
```

Armed where `mark_embedded` runs (`src/jobs/embed.rs:280`), i.e. only once the
vector is actually in the index. The unit:

1. calls `neighbours(id, per_point)` — one HTTP call, no inference;
2. drops hits below `review_min`;
3. passes the rest through `classify_pair(core, &a, &b, score)`.

**A pair at or above `auto_supersede` must not reach the dedupe queue.** That
band is settled for free by clustering, and letting the relate unit file it as an
ordinary pending pair would spend a model call on exactly the case where the
cheap rule is already correct — the inversion §9 exists to prevent. It also must
not be superseded pairwise here: `Clusters` (`src/jobs/consolidate.rs:42`) exists
because resolving one pair at a time leaves A pointing at a B that is itself
hidden.

So `classify_pair` routes such a pair to `NearIdentical`, and the sweep's cluster
pass reads stored `NearIdentical` pairs **in addition to** the `near_pairs`
result. That is an improvement in its own right: the cluster pass gains a durable
input instead of depending entirely on what one sampled round trip happened to
return.

Step 3 is the real work of this section. That logic currently lives inline in
the body of `consolidate::run` (`src/jobs/consolidate.rs:317–419`): the
containment check, the fact-token filter, `record_pair` / `record_settled_pair`.
It moves into one named function that **both** producers call. Two discovery
paths with two copies of the rules is the kind of divergence you only notice
when the outcome starts depending on which path saw a pair first.

A separate unit rather than a tail on `embed_batch`: a failing Qdrant query
would otherwise fail the embed job, whose retry pays for the embedding again.
Two failure domains, two units — the same reasoning that already split judge
calls out of the sweep.

### 5.3 Why this is complete

For a pair (X, Y), the member embedded **second** finds the other. When X's unit
runs, Y is either already indexed — X finds it — or not, and Y finds X later.
Embedded in the same batch, both units run after the shared `upsert` and both
see each other. There is no window in which a pair falls through, provided both
units run.

Coverage becomes **1, independent of N**, at the cost of one Qdrant query per
artifact. Under this project's economics that is free.

Re-embeds re-arm the unit: when a vector moves (model change, `resurface`), its
old neighbourhood is no longer the right one.

### 5.4 What the sweep becomes

It stays, loses its primary role, and keeps five:

| Job | Why it stays in the sweep |
|---|---|
| Backlog | Artifacts embedded before this feature never had a `Relate` unit |
| Backstop | Units that exhausted their retries leave holes nothing else sees |
| ≥ `auto_supersede` clustering | Union-find spans many pairs at once; inherently a batch operation. Its input is now `near_pairs` **plus** stored `NearIdentical` pairs (§5.2) |
| Lifecycle repair | See §5.5 |
| Unfinished merges | See §8.1 |

`sample` stops being a correctness parameter and becomes the rate at which the
backlog is worked off.

### 5.5 Lifecycle repair moves from scanning to marking

`repair_lifecycle_drift` scans both stores with `DRIFT_SCAN = 5000`
(`src/jobs/consolidate.rs:92`). Autonomous merging makes hidden artifacts grow
monotonically — every merge hides at least two, forever — so the cap is reached
permanently and the repair degrades into sampling an ever-growing set.

`artifacts.lifecycle_dirty` is set in the same SQLite write that changes
`status`/`superseded_by`, and cleared once the Qdrant write is acknowledged. The
repair then reads `WHERE lifecycle_dirty = 1`: O(open writes) rather than
O(hidden artifacts), and without the two mutually-offset scan windows that
produced the bug pinned by `a_scan_cap_reached_from_both_sides_is_not_drift`.

The existing full scan is retained as an infrequent reconciliation — it catches
drift that arose with no SQLite write behind it — but no longer runs every
sweep.

## 6. The dedupe contract

Renaming, to end a genuine collision: `/ui/judge` and `src/web/judge.rs` are the
relevance-feedback evaluation surface, unrelated to this. This subsystem is
renamed to match what it does.

| Before | After |
|---|---|
| `src/jobs/judge.rs` | `src/jobs/dedupe.rs` |
| `Stage::Judge` | `Stage::Dedupe` |
| `consolidate.judge` | retired; `max_dedupe_per_tick = 0` stops the asking |
| `JUDGE_SYSTEM` | `DEDUPE_SYSTEM` |

### 6.1 Four outcomes

| Verdict | Meaning | Effect | Synthetic text? |
|---|---|---|---|
| `distinct` | different subjects | `NoConflict`, both stay | no |
| `conflict` | same subject, differing value, no clear direction | `Contradiction`, both stay, **escalated to Ops** | no |
| `replaced` | one plainly replaces the other | `supersede`; the survivor is a stored original | **no** |
| `duplicate` | same subject, same claim, complementary detail | merge; both roots superseded | yes |

`replaced` is preferred over `duplicate` wherever it applies, and the prompt says
so. The survivor is then a verbatim stored artifact with a valid `corpus_span` —
strictly better than a rewrite, and the path by which §3's fidelity argument
keeps holding under autonomy. A merge is the answer only when *both* sides carry
something the other lacks.

The existing direction guard survives unchanged: a verdict naming the **newer**
artifact obsolete is rejected and falls back to `conflict`
(`src/jobs/judge.rs:96–102`).

### 6.2 One call per component

```json
{
  "relation": "duplicate" | "conflict" | "replaced" | "distinct",
  "detail": "...",
  "supersedes": "a",
  "merged": { "title": "...", "text": "...", "category": "...",
              "tags": [], "caveats": [] }
}
```

One call, not two. Asking for classification and merged text separately doubles
the only scarce resource in the system. The known risk — that a model asked for
a merged text is likelier to answer `duplicate` — is addressed by the
verification in §6.4, which costs nothing, rather than by a second call.

`merged` must be null unless `relation` is `duplicate`; a `merged` block on any
other verdict is a parse error and the reply is treated as unreadable.

**Carried over from `JUDGE_SYSTEM` unchanged**, because it was expensive to
learn: the subject check comes first and uses the titles. The comment at
`src/infer/prompt.rs:149–154` records the case — an artifact titled "FAT32
Specifications" whose body opens "32 Bit Clusternummern" and never names FAT32
again. FAT12/FAT16/FAT32 sit at 0.91 similarity and every number in them
differs. Without this rule the autonomous path merges a reference document into
mush. It gets a test that pins it.

### 6.3 Components, not pairs

The unit's target stays an `artifact_pairs` id. At **run time** it expands to the
connected component of `Pending` pairs containing that pair — `Pending` only, so
that a settled, dismissed or `NearIdentical` pair never drags an already-answered
artifact back into a group. The component is computed fresh, not snapshotted at
arming time. The `Clusters` union-find already in
`src/jobs/consolidate.rs:42` is reused.

If a sibling pair's unit also runs, it finds its pairs settled and its artifacts
superseded and becomes a no-op — the same race guard `src/jobs/judge.rs:38–47`
already applies to status changes. No `merge_groups` table is required.

**Fan-in cap.** Before the call, `COUNT(DISTINCT root_id)` over the component is
checked against `merge_max_roots` (default 8). Above it, nothing is merged: the
pairs move to `Oversized` and surface on Ops. A merge of 40 roots is no longer
one atomic piece of knowledge, which is what `schema.sql:51` defines an artifact
to be.

### 6.4 Verification — what makes autonomy defensible

Both checks are local and free. If either fails the merge is **discarded** and
the component is escalated as `conflict`, with the reason in `detail` — "the
merge would have lost `1.21.4`" is a usable line for an operator;
"verification failed" is not.

**1. No value may disappear.**

```
fact_tokens(merged.text ∪ merged.caveats)  ⊇  ⋃ fact_tokens(root.text)
```

If the model answers `duplicate` but quietly drops one side of a value conflict
while writing, the value is missing from the output. That is the one way this
feature can destroy knowledge without anyone noticing.

**2. No literal may disappear.**
`verify::missing_literals(root_text, &root_caveats, &merged.text)`. The
signature is `(artifact_text, caveats, haystack)` (`src/infer/verify.rs:94`), so
the existing function applies **unmodified** with the merged text as the
haystack instead of the segment. It catches paraphrased commands and paths; the
module header explains the stake: "a paraphrased command is a command that later
gets pasted into a root shell."

### 6.5 `may_disagree` changes role

It stops being an admission gate — under §1 that would hide the best merge
candidates from the model permanently. It becomes:

- a **prior in the prompt**: the differing values are named, and the model is
  asked whether they are a conflict about one subject or the same subject
  described at different levels of detail.

`fact_tokens`, the function underneath it, keeps carrying **verification 1**
above. Only the `may_disagree` predicate loses its job; the tokeniser gains one.

Consequence: substantially more pairs warrant a call. §9 absorbs this.

### 6.6 Re-merge always rewrites from captured roots

A merged artifact is freely eligible for further merging. When it is merged, the
model is given the **captured roots** of the whole component — resolved through
`artifact_sources` — and never a merged artifact's text.

```
gen 1:  a, b            -> M1  [roots: a, b]
gen 2:  component {M1, c}
        roots -> {a, b, c}
        model reads a, b, c   (never M1)
        -> M2  [roots: a, b, c];  M1 and c superseded by M2
```

Information loss stays exactly one generation deep, permanently. This is the
whole reason lineage is stored as a closure.

### 6.7 Acting on a verdict

Every verdict is acted on: `replaced` and `duplicate` are applied directly,
`conflict` and `Oversized` go to a person. The operator keeps undo and the
conflict queue. The verification in §6.4 and the recovery paths in §8 are what
carry that; they are not optional refinements. (An `autonomous` switch existed
during the roll-out — verdicts recorded, nothing applied — and was retired once
the contract had been observed on real data; a base carrying `would_merge`
rows from that period has them re-opened on upgrade.)

## 7. Ops surface

- **Merged artifacts** list, with roots, verdict detail, and **Undo merge**.
- **Conflicts** — `Contradiction` pairs. The one queue that expects a human.
- **Oversized** — components past the fan-in cap.
- Detail pane: a `merged` artifact renders **its roots**, each linking to its own
  corpus, where a captured artifact renders corpus lines from `corpus_span`.
  `provenance` selects the branch.

## 8. Write path and recovery

A merge is five steps across two stores that cannot be written atomically — the
same underlying problem that makes `repair_lifecycle_drift` necessary at all.

```
1. insert M (provenance='merged', corpus_id NULL, embed_state pending)
2. insert artifact_sources rows for every root
3. embed M
4. supersede each root onto M   (SQLite, then Qdrant)
5. settle the component's pairs
```

**1 and 2 in one SQLite transaction.** An M with no lineage rows is an artifact
whose detail pane can render nothing and whose roots nobody can recover — and
§6.6 depends on those rows. `insert_artifacts` already uses `tx`; this extends
the pattern rather than introducing one.

**3 before 4**, which is the deliberate departure from the obvious order.
Superseding the roots before M is indexed opens a window in which the roots are
out of search and M is not yet in it — the knowledge is **temporarily
unreachable**. That is the failure class `deleting_the_survivor_puts_the_artifact_it_hid_back`
(`src/jobs/consolidate.rs:883`) and `heal_dangling_supersessions` exist to
prevent. In this order every interruption is benign:

| Interrupted after | State | Visible? |
|---|---|---|
| 1+2 | M active but unindexed, roots active | roots in search — the pre-merge state |
| 3 | M indexed, roots still active | M **and** roots in search — redundant, nothing lost |
| 4 (SQLite) | roots hidden, payload lagging | `lifecycle_dirty` (§5.5) catches up |
| 5 | applied, pairs still open | a sibling unit finds settled artifacts, no-ops |

At no point does a statement leave search. The worst case is redundancy, which
is the state the system is coming from anyway.

### 8.1 Finishing an interrupted merge

Step 3→4 hangs off the same hook as §5.2: when an artifact with
`provenance = 'merged'` finishes embedding, its roots are superseded. A crash in
between leaves row 3 of the table standing, so the sweep gains one repair, built
on the model of `heal_dangling_supersessions`:

> **Merged artifacts whose roots are still active** — a join of
> `artifact_sources` against `artifacts.status`. Cheap, and the only thing that
> would ever notice this state.

### 8.2 Undo, and the trap in it

**Undo merge**: reactivate the roots (`Core::reactivate` exists), set M to
`deprecated` — not deleted, because `artifact_sources` cascades away with it and
takes the record of what was attempted.

That alone accomplishes nothing. The next sweep re-finds the reactivated roots,
the model says `duplicate` again, and the operator's decision is silently
undone. This is literally the bug pinned by
`reactivating_a_superseded_artifact_survives_the_next_sweep`
(`src/jobs/consolidate.rs:700`).

So the undo **must also set the component's pairs to `Dismissed`**, in the same
action. `record_pair` is `INSERT OR IGNORE` (`src/store/pairs.rs:109`) and
respects that permanently — the mechanism exists and only needs to be used.

Important distinction: this applies to an **explicit undo**. If M is simply
*deleted*, `heal_dangling_supersessions` restores the roots on its own, and a
fresh merge is then correct, because the duplication is genuinely back. A
decision may overrule the sweep; a deletion may not.

### 8.3 A deleted root

`artifact_sources.root_id` cascades. Deleting a corpus removes its artifacts and
with them lineage rows of M — while M's *text* still carries the content. M then
claims less provenance than it has.

Not data loss, but a silent untruth, and there is already a field for it:
`artifacts.flags` / `flag_detail`, used today by verification. The sweep repair
sets `orphaned_source` on M, and the detail pane says "one of this artifact's
sources was deleted" rather than quietly omitting it. No new field, no new write
path.

### 8.4 Retries and dead endpoints

Unchanged — the existing mechanism fits without adaptation.

- Unreadable reply → `record_unreadable_judgement`; component stays open; the
  unit retries under the queue's backoff.
- The prompt carries the attempt number. Not optional against this endpoint:
  identical prompts replay identical cached output, so an unchanged retry would
  return the same unreadable bytes five times.
- At `MAX_UNREADABLE_JUDGEMENTS` the asking stops, the component stays `pending`
  and therefore on the Ops list. No merge is left half-applied.
- Endpoint down → `gate.background()`, breaker, no merges. Because the write path
  begins only **after** a successful and verified reply, an outage cannot produce
  a partial merge.

## 9. Budget and pacing

`max_judgements = 20` bounds what **one sweep** arms. That was right while the
sweep was the only producer. After §5 it is not, and after §6.5 the gate that
kept most pairs away from the model is gone. A number per 24-hour tick is then
not a budget but a queue that only grows.

The fixed quantity in this system is neither the base nor the sweep — it is
**hardware throughput**. So the budget becomes a rate:

```toml
[consolidate]
dedupe_interval_mins  = 15   # own ticker, independent of the 24h sweep
max_dedupe_per_tick   = 5    # => ceiling of 20 calls/hour
merge_max_roots       = 8
```

A dedicated ticker on the model of `spawn_retention_ticker`; the comment at
`src/jobs/consolidate.rs:193` records why retention was lifted out of the sweep,
and the same argument applies — the pacing of dedupe calls has nothing to do
with the rhythm of duplicate discovery.

Two existing rules are explicitly **not** touched:

- **No cap on in-flight units.** `src/jobs/consolidate.rs:455–461` explains why:
  a unit the queue cannot get through — open breaker, dead endpoint — would under
  an in-flight cap block every other pair permanently. That is exactly the
  head-of-line blocking the units were introduced to remove. The protection stays
  `live_job` plus the ordering in `pairs_to_judge`.
- **Queue ordering.** Dedupe units are armed with a high `seq` so they sort
  behind synthesis and embed work of equal attempt count. `jobs.seq` and
  `idx_jobs_claim2` already support this (`src/store/schema.sql:137`). Dedup then
  consumes what capture leaves over, never the reverse: a large ingest may delay
  hygiene; hygiene may not delay an ingest.

## 10. Configuration

```toml
[consolidate]
enabled               = true
near_dupe_min         = 0.90
review_min            = 0.88
auto_supersede        = 0.95
per_point             = 5
interval_hours        = 24
dedupe_interval_mins  = 15
max_dedupe_per_tick   = 5      # replaces `max_judgements`
merge_max_roots       = 8
```

`README.md`'s configuration table and `config.example.toml` are updated,
including a comment stating plainly what happens by default.

## 11. Testing

Test names in this project are sentences carrying the bug they pin in a comment.
In that form:

**Lineage and re-merge**
- `a_merge_of_a_merge_is_written_from_the_captured_roots` — the core anti-drift
  rule; M1(a,b) + c means the model sees a, b, c and never M1's text.
- `the_lineage_of_a_merged_artifact_names_only_captured_artifacts`
- `a_component_past_the_fan_in_cap_is_not_merged`

**Detection**
- `an_artifact_finds_its_duplicate_the_moment_it_is_embedded`
- `a_pair_is_found_by_whichever_member_is_embedded_second`
- `the_sweep_and_the_relate_unit_classify_a_pair_identically`
- `a_re_embed_looks_for_neighbours_again`

**Contract**
- `two_artifacts_about_different_subjects_are_never_merged` — the
  FAT12/FAT16/FAT32 lesson (`src/infer/prompt.rs:149–154`). The most important
  test in the set.
- `a_value_conflict_is_escalated_and_never_merged`
- `a_plain_replacement_supersedes_rather_than_merging`
- `a_direction_naming_the_newer_artifact_is_not_trusted` — existing, must survive.
- `a_merge_that_drops_a_value_is_refused`
- `a_merge_that_paraphrases_a_command_is_refused`
- `a_pair_with_no_differing_values_still_reaches_the_model` — the polarity change.
- `a_merged_block_on_a_non_duplicate_verdict_is_unreadable`
- `a_near_identical_pair_never_costs_a_model_call` — §5.2's routing. The
  regression it guards against is the expensive one: the free band quietly
  becoming a paid one.

**Write path and recovery**
- `knowledge_is_never_unreachable_during_a_merge` — §8's invariant, checked by
  interrupting at each of the five steps. The most valuable test in the design.
- `a_merge_whose_roots_were_never_superseded_is_finished_by_the_next_sweep`
- `undoing_a_merge_survives_the_next_sweep`
- `deleting_a_merged_artifact_puts_its_roots_back`
- `deleting_a_root_flags_the_merged_artifact_rather_than_hiding_the_loss`
- `a_second_unit_for_the_same_component_is_a_no_op`

**Pacing**
- `dedupe_never_runs_ahead_of_capture`
- `the_dedupe_ticker_holds_its_rate`

Merge replies are scripted through `ScriptedCompleter` (`src/infer/fake.rs`), as
the existing judge tests already do.

**Two existing tests are deliberately rewritten**, named here so it does not
disappear into a diff. Both encode the old polarity and are made wrong by §6.5:

- `a_pair_with_nothing_to_disagree_about_never_reaches_the_queue`
  (`src/jobs/consolidate.rs:1432`)
- `a_pair_with_no_facts_to_disagree_about_never_reaches_the_model`
  (`src/jobs/consolidate.rs:1285`)

Both assert that a pair with no differing values is settled. It is now the best
merge candidate. Their replacements carry a comment stating what used to hold
and why it no longer does.

### 11.1 The evaluation harness

`tests/eval.rs` scores query/artifact pairs. Merging changes **what is
retrievable at all**: a merged artifact stands where two originals stood.
Whether that raises or lowers hit quality is a measurement, not a design
question.

The harness is extended to handle merged artifacts — a graded pair whose target
artifact has since been superseded by a merge counts as a hit on the merged
artifact, since that is where the knowledge now lives; without this the score
collapses for a reason that says nothing about retrieval. Scores are then taken
before and after a merge run over the same corpus, and the delta is recorded in
the implementation plan. A drop is a finding about the feature.

## 12. Rollout

1. Schema and migration (§4). Requires recreating the database — the schema
   header's stated path during testing.
2. Detection and the lifecycle-repair rework (§5). Independently useful, changes
   nothing about fidelity, and it is the foundation everything after it stands
   on.
3. Rename and the dedupe contract with `autonomous = false` (§6). Verdicts are
   recorded; nothing is applied. The recorded verdicts are read before going on.
4. The merge write path, verification, and recovery (§8).
5. Ops surface (§7), pacing (§9), eval harness (§11.1).
6. Flip the default to `true`.

Steps 3 and 6 are separated on purpose: the observation window between them is
the cheapest evidence available about whether the contract in §6 holds on real
data.

## 13. Risks

- **The model merges two distinct subjects.** Mitigated by the subject-first
  prompt rule and its test; residually caught by undo. The FAT32 case is the
  known shape of this failure.
- **Merged text is worse retrieval material than either original.** Not
  detectable by any check in §6.4 — it is a quality question, and §11.1 is the
  only instrument for it.
- **Closure rows and edges disagree after a deletion.** Accepted with the
  `orphaned_source` flag (§8.3) rather than resolved.
- **A default of `true` means a shipped instance rewrites its own knowledge
  base without being asked.** Every mechanism in §3 and §8 exists to bound this.
  It remains the largest single risk in the design, and it is a deliberate
  choice, recorded as such.
