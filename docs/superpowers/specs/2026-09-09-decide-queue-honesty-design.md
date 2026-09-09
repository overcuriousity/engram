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

The cause is visible in `dedupe_schema()` (`src/infer/prompt.rs:2224`) and in
the shape `DEDUPE_SYSTEM` shows:

```json
{"verdict": {"relation": "duplicate", "detail": "...", "merged": {…}}}
```

`relation` comes first, in the example and in each of the four `anyOf`
variants' `properties` and `required`. The response format is sent with
`"strict": true` and compiled into "a grammar the decoder cannot leave"
(`src/infer/openai.rs:888`). The model is therefore required to commit to the
label before it has written a word of justification — and the justification,
written second, is where the better reasoning shows up.

A string comparison between the two fields is not the fix. Agreement between a
label and a sentence of free prose is a semantic question over nondeterministic
text, and deterministic token matching answers it badly; the tree already paid
for that lesson once, in the `infer::facts` post-mortem recorded above
`dedupe_prompt`. The fix belongs at the inference layer.

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

A new route beside the two that already post to a pair:

```
POST /ui/ops/pairs/{id}/synthesize
```

in `src/web/ops.rs`, calling `crate::jobs::merge::write` — the same machinery
`dedupe.rs` uses for `Relation::Duplicate`. It writes a `Provenance::Merged`
artifact, records `artifact_sources`, supersedes both sources into it and
journals a `Kind::Merge` action per source. Undo already exists at
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

### F2 — reason before verdict

In `dedupe_schema()`, move `detail` ahead of `relation` in every one of the
four `anyOf` variants, in both `properties` and `required`, and update the
example shape in `DEDUPE_SYSTEM` to match. The model then states why before it
names what, and the label is conditioned on the reasoning rather than the
reasoning on the label.

`parse_dedupe` needs no change — it deserializes through serde and is
order-independent. Only generation is affected.

This is prevention rather than detection, and it costs no additional call. A
second inference pass checking prose against verdict remains available if this
proves insufficient, but at two merges in the base's whole history, a
verification call on every verdict is a poor trade against a divergence the
ordering may remove outright.

**Risk, stated plainly.** The four variants are discriminated by `relation`.
With `detail` first they share a common prefix, and the branch is not decided
until `relation` arrives. Grammar compilers handle common prefixes, but the
tree's own warning applies — "A grammar is only as good as the endpoint
honouring it, and `structured_output` can be switched off"
(`src/infer/prompt.rs`). This must be verified against the live judge before
it is committed. If the endpoint will not honour it, F2 falls back to
instructing the ordering in `DEDUPE_SYSTEM` prose alone, and the F3 safety net
below becomes necessary.

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
  way `apply_pair_supersede_ui` does, and refuses an id that is not part of
  the pair.
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
