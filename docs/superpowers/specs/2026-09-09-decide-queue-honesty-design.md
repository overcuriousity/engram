# The decide queue says things that are not true

Written 2026-09-09, from the live tenant base
(`~/engram/data/users/b92f3671f8808fa0.db`) rather than from the tree. Every
count below was read off that database on the day of writing.

The Capture page currently shows 24 cards, and every one of them says **"these
two disagree"**. Not one of the 24 is a disagreement. Nothing ever judged them.
This spec says why that happens, what else fell out of looking, and what to
change.

## The defect

Three symptoms, one cause.

`src/web/templates/_decide.html:35` picks the card's sentence from the pair's
state:

```
{% if p.vacuous %}neither of these says anything
{% elif p.contradiction %}these two disagree
{% else %}these two cover the same ground{% endif %}
```

`Contradiction` renders as a factual claim about the two artifacts. But
`src/jobs/dedupe.rs:200` writes that state for something else entirely:

```rust
return settle(
    core,
    &p,
    PairState::Contradiction,
    Some("These cannot be merged automatically: what one of them is made of is \
          stored source text, and a merge must not rewrite that. Resolve by hand."),
).await;
```

That branch runs **before the model is called**. Its own comment is candid
about the overload:

> `Contradiction` and not `Dismissed`: these two may well say the same thing,
> and that question stays open on somebody's queue. What is unavailable is only
> the automatic answer.

So the state means "could not be merged automatically" and the page renders it
as "these two disagree". All 24 rows in the live base carry that exact detail
string, and all 24 have `judge_attempts = 0`.

### Why every pair lands there

A passage is its own root, and 3750 of the base's 3933 artifacts are passages,
so the lineage check above refuses nearly everything it sees. 22 of the 24
pairs are passage/passage.

That should not have been possible. Both known producers already exclude
passages — `src/jobs/relate.rs:45` refuses to anchor one, and
`src/jobs/associate.rs:535` refuses to hand a passage duplicate to
consolidation. The pairs exist anyway because there is a **third producer that
has neither guard**: `src/jobs/sleep.rs:446`, `interference()`.

The timestamps settle it. All 24 pairs were created on 2026-09-09 at 03:15:14,
09:15:29 and 15:15:40 — the three sleep runs of that day, to the second. These
are not rows an older base filed.

### The 0.0 score is the same function

`interference()` fills the score like this:

```rust
let score = core.vectors
    .neighbours(&owner.id, core.consolidate.per_point).await?
    .into_iter()
    .find(|h| h.payload.artifact_id == x)
    .and_then(|h| h.similarity)
    .unwrap_or(0.0);
```

When `x` is not among the owner's top-`per_point` neighbours, `find` returns
`None` and `unwrap_or(0.0)` invents a zero. Seven of the 24 carry it.

Meanwhile `src/web/ops.rs` reads an exact zero as a different fact entirely:

```rust
let via_link = p.score == 0.0;
```

and the card then says "no similarity was measured". The comment there reasons
carefully about why an exact float zero out of a real embedding is
vanishingly unlikely, and concludes the marker can stay implicit. That
reasoning was sound about the two producers it knew about. `interference()`
manufactures the sentinel out of a failed lookup, and two meanings now sit on
one value.

## What is actually in the queue

The 24 were read individually. The content is worth recording, because it
explains what `interference()` measures and why that is the wrong input for
this page.

**Genuine semantic contradictions: none.** The closest candidate — pair 518,
where one artifact expands PUID as "Personal User ID" and the other as
"Passport Unique Identifier" — differs in the expansion of an acronym and
agrees on everything else: a system-assigned unique identifier per Microsoft
account. One side is simply wrong (the historical expansion is Microsoft
Passport's). That is a correctness error inside one artifact, not a conflict
between two.

**Genuine duplicate pairs: none.** Pairs 521, 522 and 523 all name the heise
audio-fingerprinting article, which genuinely was captured twice — but each
pair joins *different sections* of the two captures, so no pair is itself a
duplicate. The duplicate sits a level up, at the corpus.

**The rest is format similarity, not shared subject.** Two Lehrbrief cover
pages (569), two tables of contents (567, 568), two slices of one bibliography
(559), and local-business listings that share an address-and-hours template
and nothing else — Baumärkte against Tierbedarf against Supermärkte (527, 528,
529), Hausärzte in Bad Aibling against Fachärzte in Rosenheim (530–534), a
doctor's listing against a veterinary practice (517).

One artifact distorts the set on its own: the Locard quotation
(`01a084e3-896a…`) appears in five of the 24, each time against an unrelated
passage of its own lecture.

This is exactly what `interference()` is built to find. It reads
`rehearsal_results` and reports artifacts that outrank an owner across every
rehearsal — **competition in retrieval**, which is precisely what documents
sharing a template produce. It is a real signal about ranking. It is not a
finding about meaning, and it should never have reached a card that makes
claims about meaning.

## The one real duplicate

Since the queue cannot answer "are there real duplicates", the corpora were
compared directly using the base's own estimator, `store::shingle::similarity`
— bottom-k MinHash over word 5-grams, reimplemented against the stored
`corpora.shingles` for all 3916 pairs of the 89 corpora that have a signature.

| | |
|---|---|
| heise audio-fingerprinting, `web` capture vs `journal` capture | **0.789** |
| next highest pair in the entire base | **0.036** |

There is nothing in between. The two heise captures are the same article read
two ways — 5282 against 5650 characters, different `content_hash`, and only
the second carries the `source_url`.

`near_dupe_min` ships at `0.90` (`src/config.rs:932`), so the check ran and
correctly declined. The threshold simply sits above the one case that matters,
and a re-capture through a different door will always differ by more than a
tenth in boilerplate.

## The verdict that argued against itself

The base holds one applied merge, of two veterinary-practice artifacts, and its
recorded justification is:

> Both artifacts describe the same veterinary practice, but A provides contact
> details, address, and hours while B only covers the scope of services, so
> **they are distinct**.

That sentence argues against the action it justifies. `src/jobs/dedupe.rs:597`
writes the judge's `detail` into every `corpus_actions` row and onto the pair:

```rust
core.store.record_action(&action(
    Kind::Merge, &s.pair, source, Some(&m.id), s.detail.as_deref(),
)).await?;
```

The model returned `relation: "duplicate"` and then wrote prose concluding the
opposite. Nothing reconciles the two, and the prose is what gets journaled and
displayed.

**The cause was not what this spec first said it was.** The original diagnosis
blamed field order: `relation` comes before `detail` in `dedupe_schema()` and in
the shape `DEDUPE_SYSTEM` shows, the response format is sent with
`"strict": true` and compiled into "a grammar the decoder cannot leave"
(`src/infer/openai.rs:888`), so the model appeared to be forced to commit to a
label before writing any justification.

Measured against the live judge, that is false. It already emits `detail`
first, with the shipped schema and the shipped example. The label is not a
commitment taken before the reasoning; it is a choice made **after** it, between
two categories that both fit. See **F2** for the numbers and for what replaced
the proposed fix.

A string comparison between the two fields would not have helped either, and is
not the shape of a fix here. Agreement between a label and a sentence of free
prose is a semantic question over nondeterministic text, and the tree already
paid for answering that kind of question with token matching once — the
`infer::facts` post-mortem above `dedupe_prompt`.

## The changes

### A — `interference()` stops filing pairs

In `src/jobs/sleep.rs`, `interference()` keeps detecting and keeps counting;
it stops writing. Remove the `record_pair_with_detail` call, the
`pair_between` guard that only existed to avoid duplicate filing, the `detail`
string, and the `neighbours` lookup that produced `score`.

`sleep_runs.interference` then counts what was observed rather than what was
filed. The reporting line in `_sleep.html` and the field doc-comments in
`jobs/tune.rs`, `jobs/retract.rs` and `jobs/retention.rs` — all of which read
"Pairs rule 3 filed for interference" — must say so.

Retrieval competition remains visible on Ops as a number. It no longer becomes
a card that claims two artifacts disagree.

### B — the 0.0 sentinel, by consequence

No code change. After A, `src/jobs/associate.rs:546` is the only remaining
producer of an exact zero, and it writes one deliberately for a link-found
pair, which is the single meaning `ops.rs` already documents. The collision
ends when the second writer does.

This is preferred over adding an explicit `origin` column, which is what
`ops.rs` names as the fix "if that ever stops being true". It stopped being
true because a third producer appeared, not because the sentinel was wrong.

### C — the 24 existing rows

All 24 are `interference()`'s, and none is actionable. Settle them
`Dismissed` with an honest reason, rather than deleting them, so they stay
readable. This is a one-off maintenance step against the production base and
runs only on explicit go-ahead, separately from the code change.

### D — `near_dupe_min` 0.90 → 0.60

A config default, in `src/config.rs:932` and `config.example.toml:475`. The
evidence is the table above: twentyfold headroom over the highest false
candidate in the live base, and comfortably under the observed re-capture.

The heise pair is then resolved once by hand. Lowering the threshold does not
retroactively park an existing corpus.

### E — the Synthese button

**Corrected after review.** The first draft of this section said the route
would call `crate::jobs::merge::write` directly. That is not possible.
`merge::write(core, draft, roots)` takes a `MergedDraft` — the finished text of
the new artifact — and today that text exists only as the `merged` field of a
judge's verdict. Someone has to write it, and that is an inference call.

Where that call goes is settled by the tree: **no UI route makes one.** Every
inference is a `Stage` on the job queue, which is where budget (`may_act`),
backoff, `judge_attempts` and the unreadable-answer count live. A synchronous
call in a handler would be the first of its kind and would bypass all of it.

So the press records intent and the queue does the work.

**The route.** `POST /ui/ops/pairs/{id}/synthesize` in `src/web/ops.rs`, beside
`dismiss`, `discard` and `supersede`. It marks the pair as one the operator
asked to have synthesized and arms a `Stage::Dedupe` unit for it. It writes no
artifact and calls no model.

**The pair.** `artifact_pairs` gains `synthesis_asked INTEGER NOT NULL DEFAULT
0`, set by that route. This is the operator's judgement, recorded: *these two
cover the same ground.* It is deliberately a separate column rather than a
`PairState`, because the pair's state still describes what the judge found,
and this describes what a person decided.

**The job.** In `src/jobs/dedupe.rs`, a pair carrying the flag takes a
different path. The verdict prompt is not asked — the judgement is already
made — and a new write-only prompt is used instead: the two artifacts, and the
instruction to write one that says everything both said. The result is a
`MergedDraft`, and from there the existing `Relation::Duplicate` tail runs
unchanged: `merge::write`, one `Kind::Merge` action per source, and
`set_pair_merged`.

This needs `SYNTHESIZE_SYSTEM`, a `synthesize_schema()` and a
`parse_synthesis()` returning `MergedDraft` in `src/infer/prompt.rs`. The
merge-writing rules already stated in `DEDUPE_SYSTEM` for the `duplicate`
branch — every number, version, date, path, flag, command and error string
survives; the text stands on its own — carry over verbatim, because they are
the same requirement.

**When it cannot be written.** `merge::write` returning `Error::Validation` is
handled as it is today: the pair is settled for a person with the existing
refusal text. The flag is cleared so the card stops promising a synthesis that
will not arrive.

**On the card.** While the flag is set and no merge has landed, the row says
the synthesis was asked for, in place of the buttons. Undo already exists at
`/ui/ops/merges/{id}/undo`.

`PairRow` gains `mergeable: bool`, computed with the same `roots_of` check
`dedupe.rs:185-187` performs: every root of both members must be `Captured`. The
button renders only when that holds, so the card never offers what the merge
path will refuse.

The invariant in `insert_merged_artifact` is not touched. After A, neither
remaining producer files a passage pair, so the button will in practice only
ever see captured pairs — which is the case the operator already hit by hand.

The confirmation dialog follows its neighbours, reading titles out of `data-*`
attributes for the reason documented there: a title with an apostrophe
escaped into a string literal inside an attribute makes the handler a syntax
error, which fails open.

### F1 — the action's reason stops being the judge's prose

`corpus_actions.detail` currently carries the judge's sentence as the
justification for the action. One field is serving two roles — what the model
thought, and why the base did something — and only the first of them can
argue against the second.

Separate them. The `Kind::Merge` rows written at `dedupe.rs:597` take a
deterministic description of the act itself, naming the survivor and how many
sources went into it; the judge's sentence stops being passed there. It is not
lost: `set_pair_merged` already carries it onto the pair, which is where a
reader looks for what the judge said, and where `_decide.html` already renders
it through `p.detail` under `decide-finding`.

Insights reads `corpus_actions` for the merge journal
(`src/web/insights.rs:750` and around it), so the line it prints changes from
the judge's prose to the act. If the judge's note is wanted there too, it is
fetched from the pair and rendered as a quotation attributed to the judge —
never as the reason the base acted.

This needs no text analysis. It removes the case where a journal line asserts
a reason that argues against the action beside it, and it holds whether or not
F2 succeeds.

### F2 — refuted by measurement, and replaced

**This section proposed the wrong fix.** It is kept rather than deleted,
because the reasoning that produced it was plausible and the measurement that
killed it is the useful part.

The proposal was to move `detail` ahead of `relation` in `dedupe_schema`, on the
theory that `strict` made the schema a grammar and forced the model to commit
to a label before writing any justification. Three measurements against the
live judge, in order:

1. **The endpoint accepts a reordered `anyOf`.** The union still discriminates
   on `relation`; the variants share a prefix until it arrives; the reply
   parsed. So the change was *possible*.
2. **It would have changed nothing.** With the shipped schema and the shipped
   example — `relation` first in both — the judge already emits `detail` first,
   every time. The premise was simply false. (`serde_json` here carries
   `preserve_order` transitively through `indexmap`, so the schema does
   serialise `relation` first as written; the model orders its own output
   anyway.)
3. **The real cause is the taxonomy, not the ordering.** Asked twelve times
   about the two veterinary-practice artifacts under the real prompt and real
   schema, the judge wrote materially the same reasoning every time — "both
   describe the same practice, A has the contact details, B has the services" —
   and labelled it `distinct` nine times and `duplicate` three. Two runs with
   near-identical prose got opposite labels.

The prompt's own definitions both fit that shape word for word: `duplicate` is
"they make the same claim, and each carries some detail the others lack", and
`distinct` is "different subjects, **or one covers something the others simply
do not**". A pair with one subject and complementary content satisfies both.

**A prompt fix was tried and rejected on its own control.** Narrowing `distinct`
to "different subjects" and naming the complementary case as `duplicate`:

| | vet pair (wants `duplicate`) | pair 529, unrelated (wants `distinct`) |
|---|---|---|
| shipped | 3/12 duplicate | 3/12 duplicate |
| narrowed | 8/12 duplicate | **5/12 duplicate** |

It moves the target case and makes the control worse — from 25% to 42% false
duplicates, on a verdict that hides two artifacts behind a third. At N=5 the
control had looked clean; that was noise, and stopping there would have shipped
a regression as a fix.

The residual finding is worth recording on its own: **even as shipped**, the
judge calls two plainly unrelated documents a duplicate in a quarter of runs.
That is not introduced by any change here. Its exposure is smaller than the
table suggests, because after **A** neither remaining producer files a pair
like 529 at all.

**What ships instead: the verdict stops acting.** `Relation::Duplicate` settles
the pair to a new `PairState::Duplicate` and writes nothing. The card already
renders anything that is neither vacuous nor a contradiction as "these two
cover the same ground", which is exactly what the verdict now claims, and
**E**'s button is the press that acts on it. The reading is the model's; the
decision is a person's.

The draft that call already paid for is dropped. A draft written under a label
nobody has confirmed is not worth keeping, and the writing pass asks for a
fresh one under a prompt that is not deciding anything.

### F3 — not built now

A second inference pass that reads the judge's prose against its own label and
withholds the verdict on divergence. Recorded here so the decision is visible;
deliberately not part of this work.

## Testing

- `interference()` files no pair, and still counts what it observed. The
  existing `interference_files_one_pending_pair_and_never_the_same_pair_twice`
  inverts into an assertion that nothing is filed.
- No pair reaches `pairs_awaiting_review` with `score == 0.0` except one
  written by the link judge.
- `pair_rows` sets `mergeable` false when either member's lineage names a
  non-captured root, and the template omits the button then.
- The synthesize route refuses a pair whose members are not both active, the
  way `apply_pair_supersede_ui` does, and refuses a pair that is not
  `mergeable`. It writes no artifact and makes no model call: after the press,
  `synthesis_asked` is set and a `Stage::Dedupe` unit is armed, and nothing
  else has changed.
- A pair carrying `synthesis_asked` takes the write-only prompt, not the
  verdict prompt, and lands a `Provenance::Merged` artifact with one
  `Kind::Merge` action per source.
- A pair carrying `synthesis_asked` whose merge is refused by
  `merge::write` settles for a person and clears the flag.
- `dedupe_schema()` orders `detail` before `relation` in all four variants;
  `parse_dedupe` still reads a reply written in either order.
- A recorded `Kind::Merge` action's detail is the action's own description,
  not the judge's sentence.
- Threshold: a fixture pair at 0.79 is parked at `near_dupe_min = 0.60` and
  not at `0.90`.

## What this does not do

- It does not make passages eligible for deduplication. `relate.rs` and
  `associate.rs` exclude them by design, and this spec leaves that stance
  alone. The consequence is that the PUID error and the heise passage overlap
  are not surfaced as pairs — the heise duplicate is addressed at the corpus
  level by D instead.
- It does not touch the `insert_merged_artifact` invariant, and so cannot
  reintroduce the merge cascade whose 15 deprecated generations
  (`source_count` climbing 2 … 16, most having lost their `artifact_sources`
  rows) are still in the base.
- It does not add a `PairState` for "needs a person, reason unknown". After A
  the mechanical refusal at `dedupe.rs:200` becomes rare, and a new state
  costs an enum, a migration and an API key for a row that will seldom exist.
  If it turns out to still fire regularly, that is the change to make, and it
  should be measured first.
- It does not repair `integrations`, where all 13 rows tagged `conflict` name
  a `nearest_id` that no longer exists and every detail is a slide-footer
  copyright date ("this one says 29.10.2024; the other says 21.04.2022", ten
  times over, and once with an empty side). 22 of the 90 `known` rows have the
  same dangling partner. That is a separate defect on a separate surface and
  wants its own spec.
