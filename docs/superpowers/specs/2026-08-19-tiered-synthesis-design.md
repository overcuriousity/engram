# Tiered synthesis: passages, promotion, pursuits

engram spends one inference call per segment at capture, always. That is the
right price for a curated base and the wrong price for a general one — it makes
you selective about what you paste, and it charges the same for a chapter you
will read a hundred times as for a manual you will consult twice.

This introduces three modes for how much inference is spent, one config key
apart. The lowest embeds the source text verbatim and calls nothing. The middle
spends synthesis only where use has shown it is worth spending. The highest is
what engram does today, unchanged.

Two things fall out that are worth more than the saving. Verbatim text in the
index makes `ask`'s literal check a statement about the knowledge base rather
than about its paraphrases. And a mode with no write-time inference makes `ask`
the whole of the read side, which is where this application was always going.

## Vocabulary

Four terms, and the third is new.

| term | what it is | where | embedded | model wrote it |
|---|---|---|---|---|
| **corpus** | what was pasted, verbatim | `corpora` | no | no |
| **segment** | a window of a corpus sized to the *synthesis* model's context; the synthesis queue unit | `segments` | no | no |
| **passage** | a verbatim slice of a segment sized to the *embedder*; the retrieval unit | `artifacts` | **yes** | **no** |
| **artifact** | model-written text | `artifacts` | yes | yes |

Passages and artifacts share one table, and that sharing is what makes this a
config key rather than a storage tier. The reading to hold:

> **`artifacts` is the index.** Every row in it is embedded and retrievable.
> `provenance` says how the row came to exist.

`provenance` gains two values, for four in total: `passage` (a verbatim slice),
`captured` (written from a segment), `merged` (written by consolidation),
`synthesized` (written from a pursuit).

Note that `store::artifacts::Chunk` is the Rust struct for *any* row in
`artifacts` — legacy naming from before the chunk/artifact rename. "Chunk" is
therefore not available as a name for the new unit, and is not used for it
anywhere.

## The modes

```toml
[infer]
synthesis = "eager"        # "off" | "earned" | "eager"
```

**`off`** — capture splits, embeds, and stops. Passages are the index. Zero
inference calls per corpus, or one vision call per image.

**`earned`** — the same capture path. Synthesis runs later, only where use has
shown it is worth it, and always supersedes what it was written from.

**`eager`** — today. One synthesis call per segment at capture.

`eager` is the default so that setting nothing changes nothing. `off` with no
`[infer.ask]` configured is a complete and coherent product state: a semantic
search engine over verbatim text with no inference at all.

| | `off` | `earned` | `eager` |
|---|---|---|---|
| what is in the index | passages | passages, plus artifacts as earned | artifacts |
| what consolidation grooms | — | the index | the index |
| image `describe` | optional | optional | optional |
| `ask` | optional | optional | optional |
| promotion | — | yes | optional (re-synthesis) |
| generation from pursuits | — | yes | yes |
| segments and corpora | source of truth | source of truth | source of truth |
| inference at capture | none | none | one call per segment |

---

# 1. Capture without synthesis

## One splitter, called twice, nested

`split_into_segments` is already a pure `(text, counter, budget) -> Vec<Window>`
with heading-first boundary preference, blank-line fallback, hard cut last, and
the most recent heading carried into every continuation window. The only thing
separating a synthesis window from a retrieval passage is the budget passed in.

```
raw_text ──split(segment_tokens)──▶ windows   → segments rows (state = verbatim)
             each window.body ──split(chunk_tokens)──▶ passages → artifacts
```

**A passage never crosses a segment boundary.** This is an invariant, not an
implementation detail: promoting a window later must cover a whole number of
passages, or the earned tier has a permanent ragged edge where a passage is
half-superseded.

**The inner split runs over the window's body, not its text.** `Window.text`
carries the most recent heading in from the previous window as its first
`carry_lines` lines — they belong to the document further up, not to this
window's `start_line..=end_line`. Splitting `text` would put that heading
verbatim into the first passage of every continuation window and then, because
the splitter carries headings, prepend it again to every continuation passage
inside the window. So: strip the first `carry_lines` lines off, keep them as
the heading (next paragraph), split the remainder. The same two-level shift
`resolve_span` already applies — `w.start_line - 1 - w.carry_lines` — maps a
passage's line offset inside the body to a `corpus_span`, and the inner
splitter's own `carry_lines` is discounted the same way one level down. Passage
spans partition the window's own lines; the carried heading occupies none of
them.

## The carried heading becomes the passage's title

`embed_text` already prepends `title` to the embedding input "because it holds
topical signal the body often leaves implicit." A passage under
`## Recovering deleted entries` takes that as its title — verbatim, from the
document, no inference. This is the single highest-leverage line in the mode:
it is most of what synthesis was buying at the embedding layer, and section 2
shows the embedder has a slot built for exactly it.

The heading is the one the splitter tracks: the window's carried heading for a
continuation window, else the most recent heading inside the window above the
passage. It is moved into `title` and does **not** stay in `text` — the
embedding input would otherwise carry it twice, and `text` is meant to be the
slice of the document the span names. `is_heading` recognises Markdown `#`
headings only, so plain text, man pages and most fetched pages yield passages
with no title, which section 2 renders as `title: none`. The leverage is real
for the sources engram mostly holds and absent for the rest; nothing is
inferred to fill the gap.

It also has a consequence that section 6 has to handle: every passage in one
section embeds with the same string in the most weighted position.

## The segment state that says "not synthesized, on purpose"

`segments.state` gains a value: `verbatim`. Capture at `off` and `earned`
writes every segment row in that state. It is not `pending`, and that is not
cosmetic: two existing readers treat `pending` as work owed.

- `window::settle` returns early while any segment is `pending`, and only the
  `finish` it guards renumbers, recomputes coverage and arms `Stage::Embed`.
  `verbatim` counts as **resolved** there, so a corpus with no synthesized
  segment settles and finishes like one where every window came back — and,
  in section 3, so does a corpus where one window was just promoted and the
  rest were not.
- The reconciliation sweep (`reconcile.rs:44`) re-arms `Stage::SegmentWindow`
  for every segment that is not `done`. Left as `pending`, every verbatim
  segment would be synthesized on the next sweep — eager through the back door,
  or a `failed` segment with its attempts spent where no synthesizer is
  configured. The sweep does not touch `verbatim`.

Promotion (section 3) moves a segment `verbatim → pending` and arms it; it
settles `done` like any other. There is no legacy base to carry, so this is a
new value in the `CHECK`-less column and a new arm in `SegmentState`, nothing
more.

## What capture does at `off` and `earned`

Split, write segments `verbatim`, write passages with `provenance = 'passage'`
(`insert_artifacts` takes the provenance instead of hardcoding `captured`),
then call `finish` directly — the same function a synthesized corpus reaches
through its last window's `settle`. `finish` renumbers, records coverage (green
by construction, below), arms `Stage::Embed` and moves the corpus to
`embedding`; the embed job moves it to `ready`. The status path is the one the
UI already knows, minus the `segmenting` stop. `partial` cannot occur at `off`
and means at `earned` what it means today: a promoted window that spent its
attempts.

**Segment size without a synthesizer.** `segment_tokens()` derives the window
budget from the synthesis model's context, and at `off` there need not be one
(section 1a). When `[infer.synthesize]` is configured the budget is taken from
it as today, so a later switch to `earned` finds windows the model can read.
When it is not, the budget is a fixed `[infer] segment_tokens` with a default
of 4096 estimator tokens. Configuring a synthesizer later whose context is
smaller than the windows already stored means re-capturing, the same posture
section 2 takes for the embedding recipe: no migration for a base that does
not exist yet.

## What runs, what does not

Runs at `off`: split, embed, `describe` (an image must become text before it can
be a passage at all), near-duplicate shingling at capture, `associate` and
`activation`. Does not run: `SegmentWindow`, `Title`, `Relate`, `Dedupe`,
`Consolidate`, `Merge`.

**Corpus title without a model.** `Stage::Title` is a real inference call. At
`off` and `earned` it is replaced by a local derivation at capture: the first
heading in `raw_text`, else the first non-empty line, truncated, written as the
corpus title. `finish` arms `Stage::Title` only at `eager`; at `earned` a
promotion therefore never spends a model call on a name the document already
gave, and `title_hint` keeps its meaning — a name a person chose.

## 1a. `[infer.synthesize]` and `[infer.ask]` become optional

`InferConfig` (`config.rs:356`) requires both roles today, and `Core` holds a
synthesizer and an ask client unconditionally. `off` with neither configured
is a product state this spec promises, so both become `Option`, with the
consequences that follow:

- `synthesis = "earned"` or `"eager"` without `[infer.synthesize]` is a
  startup error, not a runtime one. `off` accepts its absence.
- `Core.synthesizer` and the ask client are `Option`; the stages that call
  them (`SegmentWindow`, `Title`, `Relate`'s judge, `Dedupe`, `Merge`,
  `Generate`) are not armed when the role is absent, and a job row that
  nevertheless names one — a base captured at `eager`, then reconfigured —
  settles with a reason rather than retrying to the ceiling.
- Section 8 follows: what is not configured is not offered.

This is the largest single piece of plumbing in the spec and is listed here so
it is planned as one, not discovered as many.

## Coverage is green by construction

The corpus-bands rule (2026-08-18) is that red means no artifact's `corpus_span`
claims a line. At `off` the passages partition the document — every line is
claimed, verbatim. There is no red, and this is a guarantee of the mode rather
than a display choice. Section 3 depends on it: promotion can only ever improve
coverage, never reduce it.

## Configuration

```toml
[infer.embed]
chunk_tokens = 384        # clamped to max_input_tokens * SAFETY (0.8)
```

It belongs under `embed` because it is sized to the retrieval unit, not to a
model's context.

**384, and why it is fixed rather than derived.** engram already derives one
size from the embedder: `core/mod.rs:149` sets
`max_artifact_tokens = max_input_tokens * 0.8`, giving 6553 by default. That is
correct because it is a *ceiling* — the prompt says "split into more artifacts
rather than exceeding it" and nothing approaches it. A passage size is a
*target*, and deriving a target from a capacity is how a 32k-window embedder
gets told that 26k-token passages are a good idea. Capacity says what fits; it
says nothing about what retrieves.

384 is chosen against EmbeddingGemma (section 2): its window is 2048, so
truncation is not the binding constraint, and size becomes a pure retrieval
question. The failure mode of an oversized passage is centroiding — a passage
holding three ideas produces a vector between three regions of the space and
near none of them — and a 308M-parameter model emitting 768 dimensions has less
capacity to hold ideas apart in one vector than a larger, wider model does.
384 tokens is roughly one and a half paragraphs of prose, which is about one
idea.

The clamp reuses the `SAFETY = 0.8` the embed path already applies, so the
splitter can never emit a passage that `split_oversize` then has to cut apart
again. A splitter and an embedder disagreeing about size is a loop with no good
end.

The unit is the estimator's. `TokenCounter` is `chars * 2 / 7`
(`budget.rs:21`), deliberately pessimistic, so 384 is roughly 1,350 characters
and somewhat fewer real Gemma tokens. Every budget in engram is in these units
already; the number is stated in them and compared in them.

This number is argued, not measured. `/ui/judge` and `cargo test --test eval`
can compare 256 / 384 / 512 over one judged pair set, and the harness is what is
allowed to change it — after the embedding recipe has landed, so the two are
not measured together.

---

# 2. The embedding recipe

EmbeddingGemma becomes the default embedder. This is a separable change that
should land and be measured on its own, because it moves the retrieval baseline
that everything else here will be judged against.

## Three defects being fixed

1. **No task prefixes.** EmbeddingGemma is trained to read them; engram sends
   bare text.
2. **Queries and documents are embedded identically.** `search.rs:703` and
   `embed.rs:243` are the same call. That is correct for `bge-m3`, which is
   symmetric by design. EmbeddingGemma is asymmetric, and that is its interface,
   not a tuning detail.
3. **The document format is engram's, not the model's.** `embed_text` joins
   `"{title}\n{text}"`; the model wants `title: … | text: …` and treats a
   missing title as the literal string `none`.

None of these error. They cost recall silently, by an amount the judge harness
records and cannot attribute.

## Model plus templates is one identity

A vector's meaning is fixed by the model *and* by the text handed to it.
Changing `document_template` invalidates every stored vector as thoroughly as
changing `model` does. There is no legacy base to migrate, so no fingerprint,
no startup guard and no rebuild path is built — but the property is real, and
editing a template later silently mixes embedding spaces. The answer then is to
drop the collection and re-capture; the day that stops being acceptable, the
generation-flip machinery for it already exists in `qdrant.rs`.

## Configuration

```toml
[infer.embed]
model            = "embeddinggemma"
dim              = 768      # full MRL width; do not truncate
max_input_tokens = 2048     # or the server's real batch ceiling, if lower
chunk_tokens     = 384

query_template             = "task: search result | query: {text}"
document_template          = "title: {title} | text: {text}"
document_template_untitled = "title: none | text: {text}"
```

`dim = 768`: Matryoshka truncation to 512/256/128 buys memory at a recall cost,
which is the wrong trade for a base whose point is retrieval quality.

`max_input_tokens = 2048` is the model's window. The existing comment is the
caveat that matters — "the server's real ceiling, not the model's nominal one" —
so a llama.cpp physical batch below 2048 is the number to use instead.

**Two document templates rather than one plus a filler.** This maps exactly onto
the `match &chunk.title { Some / None }` that `embed_text` already is, and the
model card states the untitled case as a literal substitution rather than an
empty field.

Templates stay in config rather than hardcoded, because `model` is configurable
and pointing at `bge-m3` must remain possible. That is three struct fields, not
a subsystem.

The three strings above are the model card's as of 2026-08-19 (`title: {title |
"none"} | text: {content}`; `task: search result | query: {content}`; 2048
context, 768/512/256/128 MRL, 300M parameters). Two things to check once at
deploy time rather than assume: that the serving stack does not prepend a
template of its own — an Ollama modelfile or a llama.cpp wrapper that does
would double the prefix, which the stored vectors would then carry forever —
and that the server's physical batch is not below 2048. The card also names a
`task: question answering | query:` prefix; `ask` does not use it, because
section 4 clusters ask vectors and search vectors together and a second query
prefix would put them in two subspaces. One query template is a constraint of
the design, not an oversight.

## Where it lands

| site | change |
|---|---|
| `EmbedRole` (`config.rs`) | three template fields plus render helpers |
| `Embedder` trait (`infer/mod.rs:66`) | gains `embed_query`; `embed` becomes document-side |
| `HttpEmbedder`, `FakeEmbedder` | render before POST; the fake exercises the asymmetry |
| `embed_text` (`embed.rs:16`) | becomes `render_document(title, text)` |
| `search.rs:703` | calls `embed_query` |

The trait split is not optional. Rendering at the call site works until someone
adds a fourth place that embeds something — and `gaps.rs` already is one.

## Envelope token accounting

`split_oversize` computes `budget = limit - title_cost` where
`title_cost = count(title)`. With a template the envelope costs tokens too —
`title: ` plus ` | text: ` is about seven — and ignoring them makes the splitter
and the embedder disagree about size. That is the loop
`a_chunk_only_its_title_pushes_over_the_limit_does_not_respawn_itself` exists to
close, reopened slightly narrower.

`title_cost` becomes `envelope_cost(title) = count(render_document(title, ""))`
at `embed.rs:368`, `:469`, and the limit checks at `:71` and `:131`. The
existing "a title that fills the limit on its own" refusal keeps working and now
correctly refuses one where the *envelope* fills it.

## One known simplification

There is one `query_template`, holding the retrieval task. `gaps.rs` is really
doing EmbeddingGemma's `clustering` task and would prefer
`task: clustering | query: `. It cannot have it: it clusters the *stored*
search-event vectors, which were necessarily embedded for retrieval. Comparing
retrieval-prefixed vectors to each other is symmetric and still meaningful. A
second recipe would mean embedding every query twice forever to improve a
feature that names clusters for a human to read.

## Tests

- query and document render differently for the same input
- a titled and an untitled passage take different templates
- envelope cost is charged: a passage that fits without the envelope and
  overflows with it splits, and does not respawn itself
- `render_document` for a passage with a carried heading puts the heading in the
  `title:` slot

Existing tests asserting the joined embedding text (`embed.rs:1702` and
siblings) are updated to the new rendering rather than pinned. They no longer
guard anything.

The default `FakeEmbedder` is symmetric — it renders with the legacy templates
— so the retrieval tests that query with `"title\ntext"` keep landing on what
they seeded; `FakeEmbedder::with_templates` exercises the asymmetric recipe in
the tests that are about it.

---

# 3. Promotion

## The unit is the window, not the passage

A passage earns promotion; the **window it lives in** is what gets synthesized.
Handing the model a 384-token passage to synthesize a 384-token passage gains
nothing — synthesis is worth paying for because it sees context the passage
lacks.

Which means promotion is not new inference code. It is today's
`Stage::SegmentWindow`, armed by evidence instead of by capture. The `segments`
rows written at capture sit `verbatim` with nothing armed, and promotion arms
one.

## The trigger

```toml
[promote]
activation_above               = 4.0   # earned
resynthesize_after_unconfirmed = 0     # eager; 0 disables, and it ships disabled
```

`activation_above` is read against `[activation]` — baseline `1.0` at
creation, `retrieved = 1.0`, `opened = 0.5`, `confirmed = 3.0`, half-life 14
days — and the test is `>=`, after the bump, with decay folded in. The
baseline decays from the moment of insert, so "one confirmation" is
`1.0·d + 3.0`, just under the line on its own; in practice a confirmation was
preceded by the retrieval and open that made it possible, `1 + 1 + 0.5 + 3`,
and clears it comfortably. Retrievals alone — `1 + 1 + 1 + 1` at best, less
with any time between them — do not, and the next rule means they are not
supposed to.

**Engagement, not exposure.** `retrieved` fires when a passage merely *appears
in a result list*. A threshold on activation alone would spend synthesis on a
passage that has demonstrably helped nobody. So the threshold is checked at
exactly two of the three bump sites: the `opened` bump in `mark_artifact_seen`
and the `confirmed` bump in the association sweep — never at the `retrieved`
bump in `mark_seen`. A passage retrieved ten times sits at eleven and is not
promoted until someone opens it; the act that promotes is always an engagement.
No stored "was opened" flag, no new table: the condition is *where* the check
lives.

**Checked at the bump, not on a sweep.** A sweep reads decayed activation, so a
passage that crossed 4.0 on Tuesday is at 3.6 by Sunday and the threshold
silently means something different depending on when the sweep runs. Checking
where `bump_activation` is called is exact. It also means no `max_per_sweep`
key: the bump *enqueues a job* rather than calling a model, and the job queue
plus `[pacing] cooldown_secs` already bound GPU load.

**Activation must actually move.** Today every `bump_activation` call is
gated on `Core::associating()` — `associate.enabled && feedback.enabled` — and
`feedback.enabled` ships `false`, so on a default install no activation ever
changes and `earned` would silently be `off`. **`feedback.enabled` becomes
`true` by default.** Search recording, association and activation are on
unless turned off; the privacy posture is opt-out, stated in the config
comment and the README where it was stated the other way. The test
`associating_requires_both_flags_the_shipped_default_has_only_one` is
rewritten to assert the new default. `earned` with `feedback.enabled = false`
is a startup warning naming the consequence, not an error — an operator may
want verbatim search with no recording, and that is `off` under another name.

## Mechanically

1. Move the segment `verbatim → pending` and set `keep_artifacts = 1` on it.
   The flag exists for re-reading a window to pick up missed lines, and it
   makes `write_segment_artifacts` **append rather than replace**
   (`window.rs:338`). Without it the promotion would *delete* the passages it
   is supposed to supersede — artifacts and passages share a `segment_idx`.
2. Arm `Stage::SegmentWindow` for `(corpus_id, segment_idx)`. Unchanged job.
3. After `write_segment_artifacts` returns, under the `corpus_lock` the window
   job takes for the step, supersede the covered passages and carry access
   forward (below). `write_segment_artifacts` takes and releases the lock
   itself, so the supersede is a second locked step in the same job, not a
   call inside the first.
4. The segment settles `done`, which clears `keep_artifacts`, and `settle`
   runs `finish` — every other segment is `verbatim`, which counts as
   resolved — so the new artifacts are armed for embedding, coverage is
   recomputed and the corpus goes `embedding → ready` again. Nothing
   promotion-specific arms the embed; the existing path does, because the
   state was chosen so that it would.

**Idempotency.** The append is the documented exception to "replace, never
append", and its documented cost — a process dying between the insert and
`done` re-runs the window and writes its artifacts twice — was accepted for an
operator-initiated re-read. Promotion makes it the common path, so the window
job pays one extra read: under `keep_artifacts`, if the segment already holds
active rows with `provenance != 'passage'`, the write is a retry and is
skipped. The dedupe sweep is no longer the thing that cleans up after a crash.

**The guard against re-promotion is the segment state**, not `provenance`: a
window whose segment is `done` never promotes again. This matters because
passages can survive a promotion, per the next rule.

## Which passages a promotion supersedes

Each new artifact has a `corpus_span` from `resolve_span`; each passage has one
from the splitter. Both are in the same coordinate space because passages nest
inside windows.

**A passage is superseded only when some one artifact's span covers a majority
of its lines.** Per artifact, not cumulative: two artifacts claiming 30% each
leave the passage standing, because `supersede` names one winner and a passage
hidden behind an artifact that holds a third of it would send the reader to
the wrong text. Best overlap wins, ties go to the lowest ordinal, and a passage
no artifact substantially claims **stays active, verbatim, in results**.

The majority floor is load-bearing. Without it, an artifact overlapping one line
of a twenty-line passage would remove nineteen lines of verbatim text on a 5%
claim. With it, promotion can only ever improve coverage, and whatever synthesis
declines to claim keeps its passage. Coverage becomes self-healing rather than
something re-read windows chase.

## Promotion at `eager` is error-driven re-synthesis

The same mechanism, a different policy, and it is already on the roadmap: an
artifact shown often and never confirmed is misleading, and is re-synthesised
*from its source segment, never from itself*.

| | `earned` | `eager` |
|---|---|---|
| trigger | `activation_above`, plus opened or confirmed | `resynthesize_after_unconfirmed` |
| meaning | "read this properly" | "this is misleading" |
| `keep_artifacts` | `1` (append, then supersede covered passages) | `0` (replace — the old artifacts are the problem) |
| result | passages become artifacts | artifacts become better artifacts |

The `eager` detector is the one the roadmap names — exposure and confirmation
counts. `hit_count` already rides in the vector payload, is already read by
`stale_candidates`, and is already excluded from scoring for the reason that
matters here: a popular result must not boost itself. An artifact whose
`hit_count` reaches the threshold with no confirmation recorded against it — in
`interaction_events`, or in a judged pair — is re-synthesised from its segment,
with before and after shown on Ops.

It ships at `0`. Re-synthesising at `eager` changes what an existing base
contains without anyone asking, so it is a default the harness moves, not this
spec.

## Carrying access forward

**This is the part that is wrong if built the obvious way.**

A passage reaches the threshold by being retrieved, opened and confirmed, and
`associate` writes `artifact_links` rows against it from co-retrieval along the
way. Then promotion supersedes it, and:

- `supersede` (`ingest.rs:528`) writes `superseded_by` and the payload lifecycle.
  It touches neither `activation` nor `artifact_links`.
- `links_from` (`links.rs:400`) requires **both** endpoints
  `status = 'active' AND superseded_by IS NULL` — so every link the passage
  earned goes dark in the same instant, including links to artifacts that are
  still active.
- The artifact replacing it starts at `activation = 1.0` with zero links.

The system's most-used material would be systematically replaced by cold
artifacts, which must then re-earn from a standing start — and being paraphrases
they may not retrieve the same way and may never re-earn. Run long enough,
`earned` becomes a ratchet that periodically resets the accessibility of exactly
the material most used, while priming and association stop firing on the
best-worn paths.

This contradicts the roadmap's own load-bearing sentence: *the trace is fixed,
access is plastic*. Access being plastic has to mean it survives a
re-expression of the content, or plastic degrades into periodically reset.

**Activation.** The promoted artifact takes the **maximum** decayed activation
of the passages it supersedes, stamped now. Max rather than sum: one search
returning three passages of one section is one piece of evidence, and summing
would let a wide passage manufacture accessibility. `decayed()` already exists.

**Links are copied, not moved.** For every link touching a superseded passage,
write the corresponding link on the artifact and leave the original row in
place. `links_from` filters superseded endpoints out anyway, so the dead rows
cost nothing at read time. The row on the passage stays because it records
what the passage earned, and because a copy is one INSERT with no reverse
migration to get wrong. Undo (below) gets its links back for free as a
consequence — for as long as the rows exist: `prune_learning_links` removes
`learning` rows whose decayed weight falls under the floor, and a dead row
decays like a live one. That is a bounded convenience, not a guarantee, and the
copy is not justified by it.

The copy is written in state `learning` unless the original was `dismissed`.
A `related` verdict was passed on the passage's text, which the artifact does
not have; `reopen_stale_judged_links` would reset it on the next sweep anyway
once it saw `judged_rev` cleared, and writing the reopened state directly is
the same result without a window in which a judge's line is shown under text
the judge never read.

Copying needs collision handling, since two passages in one window can link to
the same artifact and both become the same `(a_id, b_id)` primary key:

- decay both weights to now via `decayed()`, take the **max**,
  set `bumped_at = now`
- `queries` takes the max, not the sum — same double-counting argument
- `cues` merge, keeping the top three by count
- re-canonicalise the pair for `CHECK (a_id < b_id)`
- `state`: a `dismissed` verdict on either side wins. The operator's "not
  related" is final, and `links_from` already treats it as an invariant rather
  than a filter.
- `judged_rev_a/b` are cleared and `state` is `learning` (or `dismissed`), as
  above.

## Ordinals after a promotion

`renumber_artifacts` orders every row of a corpus — superseded passages
included — by `(segment_idx, ordinal, rowid)`, and `finish` runs it after
every promotion. Inside a promoted window the new artifacts, the passages they
superseded and the passages that survived are therefore interleaved in one
ordinal sequence, and "the row at `ordinal ± 1`" stops meaning "the
neighbouring text". Three readers depend on document order and each gets a
definition that survives this:

- `adjacent_artifacts` (`artifacts.rs:444`) becomes "the nearest **active**
  row by ordinal on either side" — `ordinal < ? ORDER BY ordinal DESC LIMIT 1`
  and its mirror — instead of `ordinal IN (n-1, n+1)`. Today a superseded
  neighbour returns nothing on that side; at `earned` that is the common case
  in exactly the windows people use.
- Stitching in `ask` (section 5) joins passages whose `corpus_span`s abut —
  `end_line + 1 == start_line` — within one `(corpus_id, segment_idx)`, not
  consecutive ordinals.
- The consolidation exclusion (section 6) is by segment, not by ordinal.

Nothing else reads ordinals as adjacency.

**What does not carry:**

- `hit_count` and `last_seen_at`. The artifact has not appeared in results, and
  claiming otherwise puts a false number in front of `stale_candidates`.
- `search_candidates` rows and judged pairs. A judged pair records *for this
  query, that passage was the answer* — a historical fact that stays true.
  Retargeting it would rewrite the eval set to say something no human said,
  contaminating the one uncontaminated measurement in the system.
  `--export-eval` keeps real ids and superseded rows stay readable by id, so the
  pair remains resolvable forever.

The rule in one line: **learned access carries forward; recorded history does
not.** That is the fixed/plastic split applied one level down.

## Undo

`unsupersede` exists. Undoing a promotion restores the passages to active,
deprecates the window's artifacts, and sets the segment back to `verbatim`. The
copied links and the raised activation stay on the artifact — an asymmetry
accepted rather than fixed, since both sides describe the same corpus lines.

## Tests

- a window promotes exactly once; a second trigger on a surviving passage in a
  `done` segment does nothing
- a passage claimed by no artifact's span survives, active and verbatim
- a passage claimed at less than half its lines survives; at more than half it
  is superseded
- promotion appends: the passages still exist as rows immediately after
  `write_segment_artifacts`
- **the promoted artifact's activation equals the max decayed activation of its
  superseded passages, not `1.0`** — the regression test for the bug above
- a link from a superseded passage resolves through `links_from` on the
  artifact, at the decayed weight
- two passages linking to the same artifact collapse to one row at the max
  weight, not the sum, with `queries` maxed and cues merged
- a `dismissed` link stays dismissed after the copy
- judged pairs still name the passage after promotion, and `--export-eval`
  resolves it
- a passage retrieved three times but never opened does **not** promote; the
  same passage opened once afterwards does
- a corpus captured at `off` has every segment `verbatim`, reaches `ready`,
  and a reconciliation sweep arms no `SegmentWindow` for it
- after a promotion the window's artifacts are `embed_state = pending` with a
  live embed job, and coverage was recomputed — with every other segment still
  `verbatim`
- a promotion re-run under `keep_artifacts` over a window that already holds
  non-passage rows writes nothing
- two artifacts each claiming 30% of one passage leave it active
- a copied link is `learning`; a copied `dismissed` link is `dismissed`
- `adjacent_artifacts` skips a superseded row and returns the next active one

---

# 4. Pursuits

Promotion answers "this passage was never read properly." It cannot answer the
other case: five passages across three corpora that jointly served one need, none
of which individually crosses anything, and where what was wanted was the
assembly rather than any one of them.

## The unit is a pursuit, not a session

A time window is the wrong unit — three unrelated searches in fifteen minutes
are three needs. Idle time bounds the *candidate set*; coherence carves it into
pursuits.

`search_events.query_vec` is already stored. Cluster a window's queries by
cosine — the same grouping `gaps.rs` already does to name knowledge gaps — and
each cluster is one **pursuit**: a coherent thing that was wanted, its queries,
and everything done with the results. Local, no inference.

The same grouping means the same line. `gaps.rs` once used a fixed `0.55` and
its header records why that failed: under bge-m3 unrelated short queries land
in 0.45–0.6, single linkage is transitive, and a few dozen of them chain into
one group. What replaced it is `link_threshold()` — the 99th percentile of
pairwise cosine over the base's own recorded queries, rounded, clamped to
`[0.55, 0.9]`. Pursuits use **that function**, not a second constant; a
`coherence` key would reintroduce the guess the repo already paid to remove,
and the embedding recipe in section 2 moves the whole cosine distribution in a
way no constant would follow.

## What gets recorded

`search_events` and `search_candidates` already capture query, candidates, rank
and shown. A new table captures what happens after the list renders:

```sql
CREATE TABLE interaction_events (
  id              INTEGER PRIMARY KEY,
  search_event_id TEXT REFERENCES search_events(id) ON DELETE CASCADE,
  artifact_id     TEXT REFERENCES artifacts(id) ON DELETE CASCADE,
  kind            TEXT NOT NULL,
  at              INTEGER NOT NULL,
  detail          TEXT
);
```

| kind | weight | why |
|---|---|---|
| `opened` | 1.0 | deliberate |
| `pivoted` | 1.5 | followed a neighbour, association or continuation — the strongest voluntary act, and unique to this application |
| `returned` | 2.0 | came back to it later in the pursuit; hard to fake |
| `confirmed` | 3.0 | already an activation delta; reused verbatim. Not a row here — it is read from `search_events.expect_id`/`verdict`, which already record it |
| `dwell` | ≤0.5, capped | tiebreak only, never decisive |
| `refined` | — | searched again without opening anything: a failure signal |
| `abandoned` | — | searched, opened nothing, no follow-up: the strongest failure signal, and the one the README already says leaves no other trace |

The pursuit itself is a row, so the analysis job has something to claim and Ops
has something to show:

```sql
CREATE TABLE pursuits (
  id           TEXT PRIMARY KEY,
  opened_at    INTEGER NOT NULL,
  closed_at    INTEGER,
  -- open | satisfied | unsatisfied | generated | dismissed
  state        TEXT NOT NULL DEFAULT 'open',
  -- Why it closed, in one line. Read on Ops; never parsed.
  reason       TEXT,
  -- The clustered queries, JSON. Becomes the artifact's `cues` on generation.
  queries      TEXT NOT NULL DEFAULT '[]',
  -- The generated artifact, once there is one.
  artifact_id  TEXT REFERENCES artifacts(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_pursuits_state ON pursuits(state, opened_at);
```

`interaction_events` rows are joined to a pursuit through their
`search_event_id`; the clustering decides which pursuit that is, so the events
carry no pursuit id of their own and re-clustering never has to rewrite them.

`dwell` is measured by the page — the detail root names its artifact, a timer
starts on every swap, and the seconds are sent as a beacon when the reader
leaves (next swap, tab hidden, page gone); under three seconds is a glance and
is not sent, over ten minutes is a tab left open and is capped. The sweep
turns it into ≤ 0.5 per artifact (a tenth per minute) and uses it for one
thing: the order the sources go into the prompt. It never counts toward
`min_engagement`, and a search whose only trace is dwell is a search nothing
was opened on. The `opened`/`pivoted`/`dwell` rows are attached to search
events by time and scope at analysis, never by a stored id; `returned` is
derived there too.

**Dwell is deliberately the weakest.** Long dwell means *this was useful* or
*this was confusing and hard to read*; a tab left open means *engaged* or *went
to lunch*. With one operator there is no volume for the noise to average out.
The failure kinds carry no engagement weight because they attach to no artifact
— they are what says the base did not answer, which is the precondition for
generating at all.

## The stopping rule

**A `synthesized` artifact at final rank 1, at or above `weak_below`, marks the
search `answered` — no `interaction_events` are recorded for it, and any
pursuit the clustering later places it in closes `satisfied` without analysis
or generation.** The mark is a flag on the `search_events` row, set at
result-assembly time in `search.rs`; the pursuit does not exist yet at that
moment — section "The unit is a pursuit" says the clustering decides which one
an event belongs to — so what is written is the fact, and the analysis pass
acts on it. The search event itself is still recorded: the judge and the gap
sweep read it as they always did.

"Final rank" is the rank the page shows — after the reranker, where one is
configured. `weak` is read from the vector similarity regardless
(`search.rs:781`), which is why the two are stated separately.

The `weak_below` qualification matters: a fused rank says where a hit placed,
not how good it was, and a generated artifact at rank 1 with a 0.31 cosine is
labelled "loose" on the page and is not a perfect answer.

This rule is not a detail. **It is what makes the loop terminate.** Without a
stopping condition every session generates, and generated artifacts seed
sessions that generate more. With it, the system generates only while the base
is failing and goes quiet the moment it is not. When a generated artifact later
goes stale it stops being rank 1, telemetry resumes, a fresh one is generated,
and consolidation resolves the pair. It self-maintains.

**A second anchor, for when rank 1 is not reached.** The top-1 rule terminates
only if the generated artifact places first for the *next* phrasing of the
need, and a paraphrase need not. So before enqueueing, the analysis pass
resolves the engaged artifacts to roots through `roots_of` and compares the set
against the roots of every active `synthesized` artifact. **If the pursuit's
roots are a subset of an existing generation's roots, nothing is generated**,
and the pursuit closes `satisfied` naming that artifact. Local, one query per
candidate, no model. This is what stops "the same assembly, again" without
asking the ranking to have noticed; what the ranking then still has to do is
surface the artifact, and consolidation gets a second chance if it does not.

A pursuit also closes `satisfied` when any hit above `weak_below` was opened or
confirmed and searching stopped, **and** fewer than `min_sources` artifacts
were engaged. Success is success regardless of who wrote the hit; the second
clause is the assembly trigger below.

## The analysis pass — local decides, the model only writes

When a pursuit goes idle, one background job, no inference:

1. group the window's queries into pursuits at `link_threshold()`
2. any event in the group marked `answered` → close `satisfied`, stop
3. sum engagement per artifact
4. **assembled?** — at least `min_sources` distinct artifacts engaged with,
   and total engagement ≥ `min_engagement`
5. **unsatisfied?** — no strong hit opened or confirmed, or `refined` ≥ 2, or
   `abandoned`
6. **wanted as a whole?** — assembled, and the engagement includes at least
   one `pivoted` or `returned` event: the operator moved *between* the
   sources, which is what distinguishes reading three answers from assembling
   one out of three
7. assembled **and** (unsatisfied **or** wanted-as-a-whole) → root-subset
   check against existing generations → enqueue `Stage::Generate` — a new job
   stage, and the only one this spec adds
8. otherwise close the pursuit, recording why

Step 4's `min_sources ≥ 2` does real separating work: **one** engaged artifact
means there was nothing to assemble, and that is a promotion case — its window,
section 3 — not a generation case. The two mechanisms partition on the number of
sources.

Step 6 is the case the section opened with. Five passages across three
corpora that jointly served one need are each, individually, a strong hit that
was opened — "satisfied" by step 5's test — and without step 6 the scenario
that motivates generation would never trigger it. The `pivoted`/`returned`
requirement keeps the trigger honest: opening three results in a row is
reading; going back to the first after the third, or following a neighbour
out of one into another, is assembling. `dwell` does not count toward it for
the reason given above.

## The generation job

One inference call, reusing `ask/retrieve.rs` for excerpt selection.

**In:** the pursuit's queries — the only place the need enters — plus the full
text of the engaged artifacts, **whatever their provenance**.

**Out:** a normal artifact — title, category, tags, text, caveats — written to
stand alone, like every other artifact in the base.

```
provenance = 'synthesized'
corpus_id  = NULL          (its corpora are derived; see section 7)
cues       = the pursuit's queries, stored and displayed
```

**It supersedes nothing.** Its sources stay active and keep ranking. The whole
justification for generating a retrievable artifact rather than a hidden cue is
that it is inspectable — and that only holds if the material it was written from
remains reachable. This is the difference from `merged`, which hides its roots
by design.

`cues` is what makes the transparency concrete. The artifact carries the
questions it was written for, shown on its detail page: not "a model guessed you
would want this" but "this was written because these three things were asked and
the base had no answer."

## Nesting

A generated artifact **may** be a source for another generated artifact. If a
pursuit engaged one — meaning the operator pivoted through it, a deliberate act
on a result they read and found worth following — its own text goes into the
prompt, unresolved.

Depth is therefore unbounded, and it is bounded in practice from the other end
by the stopping rule: if a generated artifact were answering well it would be
rank 1 and nothing would generate at all. Chains grow only along paths a human
walked and found half-right.

**Lineage keeps its invariant regardless of depth.** `artifact_sources` stores
`root_id` naming a captured artifact and `via_id` naming the direct parent
through which it entered. A generation `G2` written from `G1` (itself written
from passages A and B) stores `(G2, root=A, via=G1)` and `(G2, root=B, via=G1)`.
So `roots_of` keeps returning captured text — which `dedupe` and `merge` depend
on and which are not part of this feature — while the chain stays fully
reconstructible through `via_id`, and "at the root there is always a corpus"
is true by construction at any depth.

The `artifact_sources` header comment currently says the closure "is what keeps
information loss one generation deep however many times a group is merged."
That stays true of **merging** and is now false of **generation**. The comment
must say which mechanism it describes.

Drift compounds along deep chains. It is caught rather than prevented:
`missing_literals` runs at every generation against the sources actually used;
the `via_id` chain makes depth visible on the detail page; and consolidation
sees generated artifacts normally, so a drifted one duplicating a fresher one
gets resolved.

## Where it shows

- **Search rail:** badged as model-written, with its source count. Never
  silently indistinguishable from captured text.
- **Detail pane:** "written from", listing roots with links — `lineage_view.rs`
  already renders this for merges — plus the cues and any literal flags.
- **Ops:** a generated list beside deprecated and superseded, with one-click
  deprecate. The existing forget-everything button covers `interaction_events`
  and `pursuits` too.

## Configuration

```toml
[pursuit]
enabled        = false   # meaningful at synthesis = "earned" and "eager"
idle_secs      = 900
min_sources    = 2
min_engagement = 3.0
```

No `coherence` key: the grouping line is measured, per the clustering
paragraph above. Off until turned on — unlike `feedback.enabled`, which section
3 makes opt-out, this generates model-written text into the index, and that is
a step an operator takes deliberately. Local only, nothing leaves the machine,
and Ops forgets all of it on one press. `pursuit.enabled = true` requires
`feedback.enabled = true`, since the events it reads are the ones recording
writes; the combination is a startup error.

## Tests

- a generated artifact at rank 1 above `weak_below` marks the search
  `answered`, records no interaction events, and the pursuit it lands in
  closes satisfied; the same artifact at rank 1 *below* it does not suppress;
  the search event itself is recorded either way
- two unrelated searches inside one idle window become two pursuits, at the
  line `link_threshold()` returns for the base
- a pursuit engaging one artifact enqueues promotion, not generation
- three strong hits opened in sequence with no pivot or return close
  satisfied; the same three with one `returned` event generate
- a pursuit whose roots are a subset of an existing generation's roots does
  not generate and closes naming it
- a pursuit that engaged a generated artifact puts **its own text** in the
  prompt, not its roots
- `artifact_sources` for that generation names the **captured** roots with
  `via_id` set to the generated intermediate
- `roots_of` on a three-deep chain returns captured artifacts only, and never a
  generated one
- no source is superseded by a generation
- a literal in the generated text absent from every source is flagged
- `dwell` alone never crosses `min_engagement`
- the pursuit's queries are stored as cues and render on the detail page
- the forget button clears `interaction_events` and `pursuits`

---

# 5. Ask

## What ask gains at `off`

Today `missing_literals` checks an answer against the excerpts it was shown, and
those excerpts are themselves synthesis — they have already lost literals on the
way. The guarantee is therefore relative: *the answer invents nothing the
artifacts did not contain*.

At `off` the excerpts are the source text. The guarantee becomes absolute:

> A command, path or version in the answer that appears in no excerpt appears
> **nowhere in the corpus**.

That is the point at which the literal check stops being a plausibility check
and becomes a statement about the knowledge base.

## Contiguous passages are stitched

Passages are consecutive verbatim slices whose spans tile a segment. When
the passages for lines 40–61 and 62–88 are both selected, they are literally
continuous text. Presenting them as two excerpts wastes tokens on the twice-repeated
carried heading, hides the continuity from the model, and cuts whatever sentence
runs across the boundary.

Before packing: group selected passages by `(corpus_id, segment_idx)`, sort by
`corpus_span.start_line`, and merge runs whose spans abut — each
`end_line + 1` is the next `start_line` — into one excerpt. Spans rather than
ordinals, because after a promotion the ordinal sequence interleaves superseded
and surviving rows (section 3, "Ordinals after a promotion") and two surviving
passages with a superseded one between them are *not* continuous text. This
restores the original text as written, costs fewer tokens than the same
passages separately, and is local work with no inference.

Two conditions keep it honest:

1. **Only for `provenance = 'passage'`.** Two adjacent *artifacts* are two
   rewrites, not continuous text; stitching them would assert a continuity that
   does not exist.
2. **Every constituent id is kept.** `ask_citations`, the literal check and the
   pursuit analysis all depend on knowing which passages actually carried the
   answer. A stitched excerpt is a presentation, not a new unit.

The existing sideways reach changes in one line. `adjacent_artifacts` is a
lookup in **document order** — the nearest active row either side, per section
3 — and is a different mechanism from `neighbours()` (vector distance,
directionless) and `artifact_links` (Hebbian association, explicitly
undirected). One step each way plus stitching is enough; `NEIGHBOUR_ANCHORS`
and `NEIGHBOUR_MAX` are ranking parameters and move only after the harness.

## Ask at `earned`: two roles

**Ask is the best pursuit signal there is.** A typed question is a better-formed
need than any search query, and `ask_events` plus `ask_citations` are already
exactly what section 4 wants: the question as cue, the cited excerpts as the
sources engaged. An ask joins the same pursuit as the searches beside it — no
second collection path.

**An abstention is the strongest unsatisfied signal available.** An answer
opening with *Not in the knowledge base* is not a weak indication like an
unopened result; it is the explicit finding that the base holds nothing. It
already feeds knowledge gaps; it additionally closes its pursuit `unsatisfied`
immediately, without waiting for the idle timeout.

**Generation is ask with a different sink:**

| | `ask` | generation |
|---|---|---|
| retrieval | `ask/retrieve.rs` — cliff, sideways reach | **the same code** |
| the need | the typed question | the pursuit's queries |
| prompt | answer and cite | synthesis prompt: atomic, self-contained |
| literal check | `missing_literals` | the same |
| sink | streamed to the page | a row in `artifacts` |

## Keep-this-answer, and a property of `off` worth naming

A kept answer becomes a corpus with `origin = ORIGIN_ASK`, which at `off` is
split into passages and embedded. Therefore:

> **At `synthesis = "off"`, the operator is the only author of model-written
> text in the index.**

There is no synthesis at capture, no promotion, no generation. The only route by
which model-written text enters the base is a deliberate click, and the trace
records as `ORIGIN_ASK` that a model wrote it. This is the sharpest form of the
fidelity line the application can have, and at `off` it costs nothing.

## The economics invert

| | `eager` | `off` |
|---|---|---|
| paid | per corpus, at capture | per question asked |
| cost of a collection never read | full | zero |
| cost of the search path | one embedding | one embedding |

The roadmap's rule — inference at write time, not read time, with `ask` as the
one carved-out exception — inverts at `off`: there *is* no write-time inference,
so `ask` is the only inference. Not a violation of the principle but its
limiting case.

## What `off` makes mandatory

`cap_per_corpus` is applied **client-side over a candidate pool of three times
the limit**, and the roadmap already names the weakness: a corpus whose
artifacts fill the pool leaves nothing to promote.

At `eager` a 10,000-token document yields perhaps eight artifacts. At `off` it
yields around twenty-six passages, and adjacent passages of a section are
additionally similar through their shared heading. One large document fills the
pool reliably and the cap has nothing left to redistribute.

**Server-side grouping (`query/groups` in Qdrant) is therefore part of this
spec**, with `cap_per_corpus` as the in-memory fallback. It is already a roadmap
item; `off` promotes it from a nice-to-have to a prerequisite. *Status:* the
fallback landed (`cap_per_corpus` over `origin_corpora`); the `query/groups`
call is on the roadmap as a prerequisite with its own measurement, and is not
built yet.

---

# 6. Consolidation

## One rule, at every mode

> **Consolidation operates on the index, not on a provenance class.**

`relate` / `dedupe` / `consolidate` / `merge` see every active row and judge
pairs on their merits. No provenance filter, no skip list. If passages are what
rank, passages are what needs grooming; a system that serves one layer and
tidies another is incoherent.

At `eager` no passage is ever embedded, so consolidation grooms artifacts by
construction rather than by rule. Same rule, different inhabitants.

## The one exclusion, and it is not about provenance

Section 1 makes the carried heading the passage's `title`, and `embed_text`
prepends it — with EmbeddingGemma's `title:` slot giving it more weight still.
Every passage under one heading therefore embeds with the same string in the
most weighted position.

**Adjacent passages in a section have systematically inflated cosine similarity
for structural reasons, not semantic ones.** Pointed at them, `relate` would
flood the pair queue with model calls adjudicating paragraphs that merely sit
next to each other — "pay only where it pays" inverted.

**Two rows from the same `(corpus_id, segment_idx)` are not pair candidates.**
Not same-corpus: *same window*. Boilerplate genuinely repeated in section 2 and
section 9 is a real duplicate and must still be found; what is excluded is
material that sits together in one window, whose similarity is an artefact of
how it was built. One predicate in `classify_pair`, beside the `in_results()`
check it already makes.

The window, not "consecutive ordinals", for two reasons. Ordinals stop meaning
adjacency after a promotion (section 3), and — the case that actually arises
at `earned` — a promoted artifact that claimed 40% of a passage leaves that
passage standing in the same window by the majority rule, and the two then
look like a duplicate pair to `relate`. Sending that pair to the judge would
spend a model call to merge, and so hide behind model text, exactly the
verbatim passage the majority rule just decided to keep. Overlap inside a
window is the window job's decision, already made; it is not the judge's.
A document short enough to fit one window loses duplicate detection inside
itself, which is the shingle path's territory at capture anyway.

One existing rule needs no change: the same-corpus containment check at
`relate.rs:148` targets a synthesis defect — one call emitting a passage twice —
and passages from a single splitter pass are disjoint slices that never contain
one another.

## `auto_supersede` stops hiding anything

`auto_supersede` currently hides a member of a cluster on a cosine threshold
alone: no model call, no `losses` check, no adjudication of any kind. And
embeddings barely distinguish negation:

> "runs on ext4"  ⟷  "does **not** run on ext4"

These sit far above any realistic threshold. The mechanism would hide one of
them by "newest wins", which here is chance — resolving by deletion a
contradiction the roadmap says goes to a person.

What remains as its benefit is near-identical content across corpora.
Byte-identical and near-identical *documents* are already caught at capture by
the shingle path, leaving only "I pasted an article, then a longer piece quoting
it" — not worth an unguarded automatic hide.

**The threshold stops being a hide and becomes a fast lane to the judge.** Pairs
above it go to the model judge like everything else, with `losses` behind them.
The config key stays (a wired feature is not legacy machinery) and changes
meaning. Cost: more model calls, bounded by the already-capped pair queue.

## The merge guards stay as they are

`losses()` is forty lines: every number, version, port, path, flag, command and
error string from both sources must appear in the merged text. Its simplification
pass has already been done on evidence — the comment records that
`missing_literals` was too strict for merges and was replaced with
`missing_machine_literals`.

Its value is highest exactly where this design points it. At `earned`,
consolidation operates over passages — verbatim source text. A merge dropping
"requires kernel 5.4+" from verbatim material destroys the one thing passages
exist to preserve. Weakening the guard while extending merging to verbatim
material would be turning two dials in opposite directions.

The unguarded path was `auto_supersede`, and it is dealt with above.

## Two consequences, decided rather than inherited

**The merge guards were written for artifacts and now apply to passages.**
`losses` holds regardless of input. Weaker is what `dedupe_prompt` relies on —
titles — since a passage's title is a carried heading, which is often exactly
the failure that prompt documents. Expect more `conflict` verdicts (escalating
to a person) and fewer confident `duplicate` ones. That is the safe direction to
fail; watch the pair queue rather than pre-tuning.

**Duplicate detection is dormant until use wakes it, and so is contradiction
detection.** Two passages in different corpora saying the same thing sit there
until each is independently promoted. On a lightly used base that is
approximately never — the deliberate consequence of paying only where it pays.
Capture no longer surfaces conflicts; use does. Pasting the same *document*
twice is still caught at capture, unchanged.

---

# 7. Origins

## The problem

`artifacts.corpus_id` is NULL for `merged`, and now also for `synthesized`. The
schema comment explains why: claiming a corpus the artifact did not come from
would put the wrong lines beside it, "the one dishonesty merging must not
commit." NULL avoids the lie by declining to answer.

The better answer is to claim **all** of them. A merged artifact belongs to
every corpus it drew from, and its parents' line ranges stay claimed — so
merging stops punching red holes in corpus coverage.

## Derived, not stored

The information already exists. `artifact_sources` names the roots, every root
is `captured`, and every captured artifact has `corpus_id` and `corpus_span`.

```rust
// Mirror of roots_of. Batched, because search needs it for a page of hits.
Store::origins_of(&[ids]) -> Map<String, Vec<Origin { corpus_id, span }>>
```

| provenance | origins |
|---|---|
| `passage` | exactly one — its own `corpus_id` and `corpus_span` |
| `captured` | exactly one — the same |
| `merged` | one per root corpus, with the roots' spans |
| `synthesized` | one per root corpus, with the roots' spans |

A join table storing the same fact was considered and rejected. Two tables
asserting one truth create a synchronisation obligation that never ends — a root
deleted, a root restored out of a merge via `artifact_sources.restored`, a merge
of a merge, an undo — and a missed site produces an artifact claiming a corpus
its lineage no longer supports, which is the original dishonesty in a new form.
Derivation cannot drift, because membership *is* lineage projected.

Behind one named function, derivation is also the future-proof choice: if stored
origins are ever wanted, the body of `origins_of` changes and nothing else. The
reverse migration would touch 250+ read sites.

## The invariant this makes expressible

> **`origins_of` returns a non-empty list for every active artifact.**

This is the enforceable, testable form of "an artifact always belongs to a
corpus" — true for all four provenances, where a `NOT NULL` column could only be
satisfied for merges by a half-truth. An empty result is the same broken state
`roots_of` already reports — a merge whose sources were deleted from under it —
and escalates to a person the same way.

## Call sites

Five, not 177. Everything else keeps reading `corpus_id` and sees no change.

1. **Corpus detail page** — a merge now appears under each of its corpora
2. **Bands and coverage** — parent spans stay claimed, the red hole disappears
3. **`cross_corpus` in `links_from`** — `x != y` becomes "intersection empty"
4. **`links_to_judge`** (`links.rs:470`) — the same test, in SQL today
   (`a.corpus_id IS NULL OR … <>`); it moves into Rust over `origins_of`
   like `links_from`, or a merge is never judged against its own parents'
   neighbours and always against everything else
5. **`cap_per_corpus` in `search.rs`** — a merge counts against each of its
   corpora

## One payload field

`cap_per_corpus` groups on `payload.corpus_id`, which is NULL for merged and
synthesized rows — so today they all land under one key. `VectorPayload` gains:

```rust
origin_corpora: Vec<String>
```

This does not contradict the rejection of a stored join table. The Qdrant
payload is explicitly a projection of SQLite — `title`, `category`, `status` and
`superseded_by` are already mirrored there, and `lifecycle_dirty` plus
`repair_lifecycle_drift` are the machinery that pulls drift back. SQLite remains
the sole authority. A second *SQLite* table claiming the same truth would have
been a different thing.

Side effect that justifies it alone: **corpus-filtered search works for merged
artifacts for the first time.** Today it cannot, because `corpus_id` is NULL.

---

# 8. What is configured is what is offered

A general rule, with precedent: without `[infer.vision]` the image door is
closed.

- no `[infer.ask]` → no ask page, no menu entry, no MCP `ask` tool
- no `[infer.vision]` → no image door (already true)
- `synthesis = "off"` → no re-synthesise buttons, no artifact filter chips that
  can never match
- `[pursuit] enabled = false` → no pursuit view in Ops
- `feedback.enabled = false` → no judge

No greyed-out control, no menu entry leading to an error page. The door is
simply not there.

---

# 9. A hazard the new provenance values create

`Provenance::parse` (`artifacts.rs:87`) is:

```rust
match s {
    "merged" => Provenance::Merged,
    _ => Provenance::Captured,     // everything unknown
}
```

Writing `'passage'` or `'synthesized'` to the column without adding them here
makes every such row read as `captured` throughout the process. The consequence
is not cosmetic: `roots_of` returns a captured artifact's *own id* as its root,
so a generated artifact would hand its own model-written text to the merge
prompt as an original — precisely the failure `roots_of`'s comment names.

Two fixes together:

- add `Passage` and `Synthesized` to the enum, `as_str` and `parse`
- `roots_of`'s inner check is a raw string compare,
  `provenance.as_deref() == Some("merged")`. It must test **"not captured"**
  rather than "is merged", so the next value added defaults to safe rather than
  to wrong. Generated artifacts always have source rows so the branch is not
  reached in practice — which is exactly why it must be right.

Audit the remaining `Provenance::Merged` comparisons for what the new values
should do: `dedupe.rs:124` (roots as prompt context — yes for `synthesized`,
same as a merge), `embed.rs:313`, `merge.rs:90/184/240`, `ui.rs:2594` and
`lineage_view.rs:295` (both need more than a boolean `merged`).

---

# Measurement

Nothing here changes a default that moves ranking without the harness having run.

Land in this order, measuring between:

1. **The embedding recipe** on its own. It moves the retrieval baseline; if it
   ships with the mode split, neither can be attributed.
2. **`off`** — then compare `off` against `eager` on the same judged pair set,
   and compare `chunk_tokens` at 256 / 384 / 512.
3. **Promotion**, then **pursuits**.

`--export-eval` reads SQLite only and keeps real ids, so re-exporting does not
invalidate judged pairs. Superseded passages stay readable by id, so pairs
naming a promoted-away passage still resolve.

# Cut

**A separate cue store or second vector for access reconsolidation.** A
generated artifact *is* the access cue, and it is the more inspectable of the
two: readable, editable, deprecable, and it says what it was written from and
which questions produced it. A hidden cue list improves ranking by means the
operator cannot see. This is a deliberate departure from the roadmap's cut of
answer cards, and it rests on two things that version lacked — the artifact is
retrievable and inspectable, and the top-1 rule makes the loop terminate.

**Similarity between passages as a promotion trigger.** Two passages being alike
is not evidence anyone needs them. Spending synthesis on redundancy nobody has
searched for is eager synthesis in disguise. Both converge anyway: each is
promoted by its own use, and consolidation then sees two artifacts.

**Per-corpus or per-capture tier selection.** The middle mode already does
per-item selectivity, dynamically and on evidence of use, which is strictly
better than a guess at paste time. A capture-time dial would be a worse version
of what the design already does.

**A migration path for the embedding recipe.** There is no base to look after.
The generation-flip machinery in `qdrant.rs` is what to build the day there is.

**`max_per_sweep` for promotion.** The job queue and `[pacing] cooldown_secs`
already bound GPU load; a second bound would be a worse re-implementation of
one that works.

**A second embedding recipe for gap clustering.** `gaps.rs` would prefer
EmbeddingGemma's `clustering` task, but it clusters stored search-event vectors
which were necessarily embedded for retrieval. Embedding every query twice
forever to improve a feature that names clusters for a human to read is not
worth it.
