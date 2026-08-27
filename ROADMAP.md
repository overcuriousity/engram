# Roadmap

Not built yet, roughly in the order it would be worth building.

engram is a memory, and from here on it is designed as an **expansion of a
biological one**. It keeps the one capability the brain lacks — verbatim recall
with provenance — and borrows the brain's mechanisms for everything that decides
how a memory is reached: association, activation, priming, forgetting, sleep.

It answers in two ways, and they are one mechanism read from two ends. A typed
question is answered semantically, out of the stored text, with the lines it
came from. And before anything is typed, the situation itself — the device, the
hour, the viewport, the network, what this sitting has already been in — is a
query the operator never had to write. The search box stays the application.

Nothing here is a screen to look at, and nothing here is a reading list. This is
not an application for revisiting what you once knew; it is one that puts the
answer to the situation you are in first, while you are still typing it, or
before. Anything whose product is "here is something you had forgotten" belongs
to a different application and has been cut from this one — see the end of
[What counts as use].

What is built: the pipeline from capture to ranked artifact; the last hop from
a ranked artifact back to its corpus lines; synthesis verified against what it
must not alter; filter chips from facet counts; nearest neighbours in the
detail pane; near-duplicate detection at capture; autonomous consolidation with
complete pair coverage, merge-loss checks and undo; caveats on artifacts; text,
image and PDF capture, the extracted markdown normalised so that detached
bullet glyphs and blank-line runs never reach the splitter; hybrid search
inside Qdrant; the evaluation harness fed from judged real searches (`cargo
test --test eval`, `/ui/judge`, `--export-eval`); Hebbian links learned from
co-retrieval with bounded priming and one-hop association in the results; the
cliff — where a ranked list's relevance falls off — marked on the rail, over
the API and over MCP; ask that streams to the page, packs to the cliff rather
than to the window, reaches one hop sideways for candidates, checks its own
answer's literals against the excerpts it was shown, and offers the answer back
as a paste the operator approves; one bounded round of planned retrieval behind
`[infer.ask] plan`, fanning out to a search per uncovered subject; named model
tiers, so a role picks a model by what the call is worth; judged questions with
a second harness — citation recall, abstention, faithfulness by literals and by
claim check; knowledge gaps grouped and named on the capture page, the fifth
kind of them the subjects a plan named and the base could not cover; what a
hit's document does next, said on the rail and read in the pane, which appends
the passages that follow one click at a time; the MCP door reaching what the
web doors reach — a link, a PDF, an image, a note, the document behind a hit,
and a meta line that says in words what the rail badges; a phone browser
offered the installed app, once a week; a
recommendation under the search box, learned from the situations an artifact
was opened in — the browser's time zone and local time, the device, the
viewport, the network and the power state, clustered per artifact and stored as
a `ctx` multivector scored with `max_sim`, with the blocks that decided each
offer named beneath it and shown-against-clicked broken down by rung on Ops. A
situation seen once or twice is offered too, saying so in words — "Twice
before" — and held to a stricter match than an established one; with nothing
learned about the situation a card is drawn at random and claims nothing; one
capture door that reads what it is handed rather than asking the client to
classify it — a body that is one link is a link, a PDF and an image arrive as
raw bytes, and a multipart share of four photos is four captures; engram in the
phone's share sheet, answering with the corpus page because that is the surface
that can say a share was held for review; and a bookmarklet and a Shortcut
recipe carrying a token minted for one device on a press, revocable on its own.

The doors that run while nobody is watching — a watched folder, directory
import, feeds, email-in — are not built, and near-duplicate parking is the
reason rather than the effort. Anything at or above `near_dupe_min` is stored
and then held: not segmented, not embedded, not searchable until a person
decides between it and what it resembles. That is right for every door where
the operator is present at the moment of capture, and it is a queue nobody will
ever drain for a door that runs overnight. A bulk door needs a bulk-safe
near-duplicate policy first, and recency decaying from a document's own date
rather than the moment engram saw it — both named in §8 of
`docs/superpowers/specs/2026-08-27-capture-doors-design.md`.

Design records live in `docs/superpowers/specs/`.

Three constraints decide what is on this list and what was cut from it.

**Inference happens at write time, not read time.** A search costs one
embedding, one vector search and — with associations on — one indexed SQLite
read; never a generation. Making retrieval better means making the background
job do more, never adding a model call to the query path. *Ask* is the one door
that generates at read time, and it is allowed to spend more than one call —
but every call it spends is bounded, visible on the page while it happens, and
whatever it learns about what retrieves well is written back so that search
inherits it for free.

**The trace is fixed; access is plastic.** Content is verbatim and never changes
silently: retrieval returns whole stored artifacts, a captured artifact is never
rewritten in place, and nothing is deleted on a score. Consolidation is the one
narrow exception and carries its own four guards (superseding preferred, merged
provenance explicit, originals kept and undoable, no value or literal lost — see
the 2026-08-14 spec). Everything about *how* an artifact is found —
associations, activation, what surfaces first — learns from use, within bounds
that are visible: a primed hit says so, an associated hit says what recalled
it, and no exact match is ever buried.

**Lean beats clever.** Anything that adds a storage tier, a model dependency or
a layer crossing without a measured retrieval gain does not go in. The harness
is the only figure comparable across months; a default that changes ranking
moves only after it has been run.

## How to read the two lines under each item

Everything not yet built carries a **Worth** and a **Cost**.

*Worth* is the difference, not the feature: what an operator or the base can do
afterwards that it could not before. Where the honest answer is "little", it
says so, and the item stays on the list at the bottom rather than being dressed
up.

*Cost* names what has to be touched and then one of three sizes. **One commit**
is a day or less, one or two files, nothing to measure. **A branch** is several
files across layers, tests of its own, possibly a migration. **A project** wants
a design record in `docs/superpowers/specs/` before any code. An item that moves
ranking carries a cost that is not code at all: a harness run, which needs a
live Qdrant, a real embedding endpoint and a corpus that is not in this
repository. That cost is named separately, because it is the one that decides
the order of this list — a seam that only joins two surfaces ships when it is
written, a seam that moves ranking waits.

## [What counts as use]

Everything plastic in the base is downstream of one question. `jobs::context`
clusters `interactions` rows into the `ctx` vectors the recommendation rests on;
`associate` replays the search log; `promote` reads an activation that only
moves at a bump. Use now means the web UI **and the API door**: asking for one
artifact by id is the same deliberate act the detail pane records, and it says
so in `get_artifact`. What `/mcp` counts as use is answered, and the answer is
nothing beyond what it already does — see below.

Built, and worth keeping the reasons for: Hebbian links from co-retrieval,
decaying activation per artifact, bounded priming and one-hop association in the
results, a sparse judge on strong cross-corpus links (`[associate]`,
`[activation]`; spec `2026-08-16-associative-memory-design.md`). Sleep, but not
as a cycle — the queue was already three-quarters of a scheduler, so it got a
priority column (`jobs.class`), ageing on the repair pass, and five fewer
tickers: a sweep arms itself one interval out when it finishes, and ordering is
expressed by arming rather than by a schedule. Units on their own periods do not
line up into one night, so there is no "last sleep", there is the last day
(`sweep_runs`, on Ops). Working memory, carrying only: a live sitting keyed by
web session (`src/core/sitting.rs`), expiring at `pursuit.idle_secs` so that the
live definition and the reconstructed one agree by construction. It joins the
doors and never writes activation, and there is a test saying so.

And engagement at more than one door. `GET /api/v1/artifacts/{id}` is an open:
it marks the artifact seen and records the interaction, with no `via` — the API
has no navigation to have pivoted through — and no session, because a bearer
token is not a conversation. A citation is an engagement: `ask_citations.used`
already separated what an answer was shown from what it used, and the artifacts
it used now bump `activation.cited` and are checked for promotion at the bump
(`Core::mark_artifacts_cited`). It deliberately does not stamp `last_seen_at`
the way an open does — the stale review list exists to put artifacts nobody has
verified in front of a person, and a model citing one is not a person having
looked at it.

**What `/mcp` counts as use: nothing, and that is the decision rather than an
omission.** Its search already bumps activation like every other door. It has no
open at all — a search returns the whole artifact, so the read *is* the result —
and counting every returned artifact as engagement would only relearn what
association already learns from display. The honest signal at that door is the
citation, and citations are recorded only where the question is: `record_ask`
stays at the web door, because a question is personal data of the same kind as a
query and API and MCP callers asked for the smallest footprint. So the bump
rides with the recording rather than around it, and there is a test pinning
that. Widening it means widening what is *recorded* first, which is a different
decision from this one.

- **Co-citation is a stronger link than co-display.** `associate` learns its
  Hebbian links by replaying the search log: two artifacts shown in one result
  list are drawn together. Two artifacts the model *cited in one answer* were
  used together — the same claim with the noise taken out. The rows exist, and
  `used` already separates shown from used.
  **Worth:** better links per unit of evidence, and an association graph that
  stops being a graph of what was displayed.
  **Cost:** a second replay source in `jobs/associate.rs` (1,579 lines today),
  a weight for it against co-display, tests. **A branch**, plus a harness run —
  links feed priming, so this reaches ranking.

- **Access reconsolidation.** A judged hit says "for this query, that artifact".
  The query becomes an additional access cue for the artifact — a second vector
  or a stored cue list — so the next similar situation finds it directly. Ask
  verdicts are the second source and the better one: a carried excerpt says the
  same thing about a question a person asked in earnest. Text untouched; it
  changes what vectors are built from.

  *There is a cheaper shape that does not wait on re-embedding.* Qdrant's
  `recommend` with `strategy: "best_score"` carries the cue as a second positive
  example at query time: the stored vector is left alone, and a candidate is
  scored by `max` over the examples rather than by their mean, so the query and
  the remembered question stay two independent ways in instead of collapsing to
  a midpoint that is neither. `average_vector` is the wrong knob for exactly
  that reason.
  **Worth:** the largest ranking gain still on this list. The second time a
  situation comes round, the artifact that answered it the first time is reached
  directly instead of re-derived from scratch.
  **Cost:** the cue store, plus either a re-embedding pass or — in the cheap
  shape — the `best_score` call in `vector/qdrant.rs`, its emulation in
  `vector/memory.rs` and a flag. **A branch**, plus a harness run. What the
  cheap shape really changes is the cost of being wrong: a flag rather than a
  re-embedding pass.

- **Error-driven re-synthesis, the judged-noise half.** The shown-often-and-
  never-confirmed trigger is built: `maybe_resynthesize` (`jobs/promote.rs`),
  wired from `core::search` once a hit is counted, re-synthesises **from the
  source segment**, never from itself, when `resynthesize_after_unconfirmed` is
  set above `0` in `eager` mode — it ships disabled. Not built: the second
  trigger, an artifact **judged noise** rather than merely unconfirmed, and the
  before/after that would let either trigger be checked on Ops rather than
  trusted.
  **Worth:** the artifacts that mislead through a bad verdict, not only through
  silence, get repaired the same way; before/after makes both triggers
  checkable.
  **Cost:** a verdict-read branch beside `maybe_resynthesize`, before/after on
  Ops, an undo path. **A branch.** No harness — it changes text through the
  guards that already exist, not ranking.

- **Usage-informed supersede.** `auto_supersede` keeps the newest member of a
  near-identical group; activation knows which member people actually confirmed.
  **Worth:** small, and only where those two disagree. First shown on the undo
  list, changed only if it turns out to matter.
  **Cost:** a column read in `jobs/dedupe.rs` and a line on the undo list.
  **One commit**, once anyone has seen it matter.

<!-- CUT: a forgotten list with a direction. The item said: replace
     `resurface`'s `{"sample": "random"}` with Qdrant context search, the
     sitting's rail as the positive side and superseded/deprecated as the
     negative, so what comes back is old and unseen *and* near what this sitting
     has been in. It is cut on the opening thesis rather than on cost: its
     product is "here is something you had forgotten", which is an application
     for learning knowledge, not one for answering a situation. The base already
     surfaces old material the only way that fits — because the situation asked
     for it, through the offer.
     Note what is NOT cut: `resurface` itself, `GET /api/v1/resurface` and the
     trait method stay exactly as they are. They are wired, cheap and answer a
     question somebody may legitimately ask over the API. What goes is the
     roadmap item — no context search, no Qdrant work, no page. If the appetite
     ever returns, the shape is not a list but a fifth rung of the offer ladder
     between `Tentative` and `Random`: situation unknown, but this sitting has a
     rail, so draw something old that is near it. It cannot replace the `Random`
     floor either way — on a base started this morning it returns nothing, which
     is the one moment that floor exists for (`src/core/recommend.rs:262`,
     `src/store/context.rs:117`).
     CUT: corpus map — the distance-matrix API over a filtered subset, plus the
     link table, drawn. Nice to look at, and by the opening thesis that is the
     whole objection.
     NOT COPIED from the brain, on purpose: confabulation (no answer cards, no
     generated answers standing in for stored text — a synthesised digest
     competes with the exact wording it was derived from, which is fidelity loss
     by design), content decay (activation fades, artifacts do not), interference
     (a new capture never overwrites an old one; a conflict goes to a person).
     These are where the expansion is deliberately better than the thing it
     expands. -->

## [Anticipation]

The half that answers before anything is typed, and the newest: an offer under
the search box, drawn from the situation the browser reports, on a ladder of
four rungs down to a random card that claims nothing (spec
`2026-08-21-context-recommendation-design.md`). Built, including the instrument
that would let it be tuned — shown against clicked, by rung, over the last
thirty days, on Ops. Everything left here but the last item is gated on that
instrument having months behind it, not on anybody's judgement; the last is
the write-time half of the same question and carries an instrument of its own.

Built: **the `scope` block is gone**, and with it the last thing in the vector
that described who was asking rather than what the situation was. It existed to
keep one person's situations from being ranked first for another while everyone
shared a collection, at weight 10 against a block total under 5. Everyone no
longer does: each user has their own database and their own Qdrant collection,
and the read path cuts foreign clusters by an exact match on top of that, which
was always the guarantee — a near-orthogonal direction is a probability, and
isolation must not be one. Inside one collection the block had become a
constant, ordering nothing and, under cosine, compressing the differences the
blocks that do describe the situation are able to make.

Two numbers became one. The full cosine ranked and `context_score` — the same
vector with that block sliced off — decided the rung, because counting it in the
gate would have dragged every same-subject pair above 0.95 and left `strong_at`
and `weak_at` four hundredths apart. With the block gone the rank and the rung
read the same evidence, and both thresholds keep the values they were calibrated
at: the gate never saw what went. What is *not* collapsed is the loop over
candidates. The rung still turns on `firm_at` as well as the score, so the
closest situation can be a cluster seen twice that fails `strong_at` while an
established pattern sits second and would pass — an argmax and a single gate is
what used to drop that page to a random card.

The cost was the one this file named and one it did not. The weights are the
encoder version, so every stored cluster is retired and the offer falls to its
`Random` floor until `jobs::context` has re-clustered. And the width changed,
45 dimensions rather than 53 — a named vector cannot be resized in place, so an
existing collection rejects the new one until `--reindex` has built the next
generation. That copies the dense vectors across and re-embeds nothing; sets of
the old width are discarded rather than reinterpreted.

- **Learned block weights.** The weights in `[recommend.weights]` are chosen,
  not measured, and the honest description of them is "chosen". Once the
  shown/clicked rate has history, they can be fitted to it.
  **Worth:** the offer stops being a guess with a good story behind it. It is
  also the precondition for the item below.
  **Cost:** a fit over `offer_rates` history and the restraint to leave the
  defaults alone until the history exists. **A branch**, gated on data rather
  than on code. Fitting them before the data exists is guessing with extra
  steps. Its one code-level precondition is met: with the `scope` block gone,
  every weight left is a weight over something that actually varies, so a fit
  is no longer a fit over a mostly-constant direction.

- **Conjunctions across scopes.** The context vector can already hold "on the
  phone the hour matters, at the desk it does not"; nothing yet learns which of
  those conjunctions are real.
  **Worth:** the difference between an encoder that *can* express a conjunction
  and a base that knows which ones hold. Unknown until the weights are fitted —
  it may turn out the fitted weights already say it.
  **Cost:** **a project**, and it waits on the item above.

- **`[sitting] prime` is unmeasured.** The live sitting can prime the next
  search, sharing the one budget `associate.prime_lift` bounds, and the flag is
  `false`. The harness has not been run either way.
  **Worth:** unknown, which is the point. It is the one default here that would
  move ranking.
  **Cost:** no code — the mechanism is built. A harness run either way, and a
  commit of its own carrying both numbers in its message. It moves that way or
  it does not move.

- **The situation as a term in ranking.** The `ctx` vectors decide the offer
  under the search box and nothing else. The moment something is typed the
  situation is dropped, and the query is one dense vector like any other
  system's. The score `recommend` already computes — `max_sim` against an
  artifact's clusters — can be a bounded term beside the recency and pinned
  terms in `scoring_formula`, so that *backup* at 23:00 on the phone need not
  rank exactly as it does at the desk on a Tuesday afternoon. Bounded the way
  priming is bounded, and never by enough to bury an exact match.
  **Worth:** the two halves of this file's opening thesis stop being two
  features that happen to share an encoder. Every piece of it is built — the
  encoder, the clusters, the sweep, the multivector in the collection — and
  search reads none of it.
  **Cost:** the ctx score reached from the search path, which is a separate
  query today, so either a third prefetch branch or the score carried across; a
  weight and a cap in `[recommend]`; the same in `vector/memory.rs`; tests.
  **A branch**, plus a harness run — and the harness needs work before the run
  is worth anything. A recorded search replayed months later is replayed in a
  different situation, so a judged pair stops being a fixed question unless the
  situation is recorded beside the query and replayed with it. That is the real
  cost of this item, and it is not the code. Gated on the block weights being
  fitted rather than chosen, like everything else in this section.

- **Speculative synthesis.** Promotion is retrospective: a window is rewritten
  once its passages have earned it. The prospective half is to spend an idle
  call on a window nobody has opened yet, chosen because the base can already
  say what is about to be asked. Three predictors are already stored and none
  of them is new machinery: the grouped **gaps**, which are a list of questions
  the base failed to answer and which recur; the **Hebbian neighbours** of what
  a pursuit just engaged, since activation spreads a hop and `maybe_promote`
  reads none of it; and the **`ctx` clusters**, which know what is opened at
  this hour on this device. Gaps are the cheapest and the most defensible —
  nearest passages to a group's centroid, one window each, at most *n* a sweep.

  Two things it must not do. It must not become `eager` by the back door: the
  budget is per idle sweep, and the instrument is a hit rate — was a
  speculatively written artifact ever retrieved, opened or confirmed within
  *N* days — read on Ops beside shown-against-clicked, so a predictor that
  cannot beat its own cost is turned off rather than tuned. And it must not
  share `Provenance::Synthesized`. That is what the pursuit stopping rule reads
  (`src/core/search.rs:1076`: a synthesized artifact at rank 1 and not weak
  means the base answered), so speculative text under it would close pursuits
  as satisfied on a guess nobody engaged, and the badge would name a use that
  never happened. It is written on speculation and says so until something
  retrieves it.
  **Worth:** the first search of a subject stops being the one that returns
  crude chunks. Everything earned today is earned by an operator who already
  went unanswered once; this is the only item that spends a call to spare them
  that, and the only one whose worth is measurable the day it ships.
  **Cost:** a predictor over stored gaps, a `Speculative` provenance and its
  migration, a per-sweep budget in `[promote]`, and the hit-rate panel without
  which none of it can be argued. **A project**, and it wants a design record
  in `docs/superpowers/specs/` before any code.

## [Ask]

Ask is the part of engram that is allowed to think, and it is built (spec
`2026-08-17-streaming-ask-design.md`). It streams to the page — reasoning tokens
included, when the model emits them — packs excerpts to the relevance cliff
rather than to the context window, reaches one hop sideways for candidates,
checks its own literals against the excerpts it was shown, and offers the answer
back as a paste the operator approves. `[infer.ask] plan` adds one bounded round
of planned retrieval and ships **on**: after the first round the model names the
subjects the excerpts miss, each becomes a search of its own, and up to three
run at once and merge into one set of excerpts. One plan, never a second, and
never a loop.

Two things it did not give up. An answer cannot carry a literal the excerpts did
not — `verify::missing_literals`, the same guard synthesis runs, applied to
generation. And nothing is written to memory without a person: the
keep-this-answer link prefills the capture box and saves nothing, so the trace
records that a model wrote the text and what it was written from.

Built: **the plan's uncovered subjects are gaps of their own**, a fifth
`GapKind` beside the four the capture page already had. A subject whose fan-out
search came back with every ranked candidate under `weak_below` is a hole the
base named itself, for a question a person asked, out of a call that was
already paid for. Weakness rather than emptiness, and deliberately the same
threshold `Unmatched` reads — one definition of "nothing near this", at a
second door. What makes it a kind rather than a second reading of `Unmatched`
is the text: a subject a model named to describe a hole, where `Unmatched`
carries a query somebody typed. Badged *planned*, and only at the web door,
because a subject is derived from a question and `record_ask` already draws
that line. Only the ranked hits are read — a neighbour reached sideways is
structure, not a match. The cost was not the "one commit" this file estimated:
it wanted a table, because planned rounds deliberately write nothing to the
search log.

Model tiers are built: named tiers under `[infer.tiers.*]`, each chat role
pointing at one, resolved at parse time into the same concrete role structs the
completers already took. The planning call runs on the efficient tier while the
answer it feeds runs on the deep one. The ask harness and ask feedback are built
too (`2026-08-17-ask-harness-design.md`): verdicts with carriers,
`questions.json` in the export, `evaluate_ask` measuring citation recall,
abstention accuracy and faithfulness by literals and by claim check, and
"nothing here" surfaced as a knowledge gap.

- **Whether the planning call earns itself, and whether packing to the cliff
  helps with no reranker configured.** A search's own fused scores are smooth
  enough that a cliff may rarely form without one; the rail has always lived
  with that and ask now inherits it. Both are harness questions, not design
  ones.
  **Worth:** either the default stays on with a number behind it, or one model
  call per ask disappears.
  **Cost:** no code. Two runs of the ask harness against a corpus that is not in
  this repository, and a commit carrying the numbers. **One commit.**

<!-- CUT: situation vectors — at ingest, the model writing the three to five
     situations an artifact answers, each embedded as an extra named vector, so
     a typed situation matches a question rather than a passage. Cut on the
     fidelity line: it puts a model's guess about what an artifact answers into
     the ranking path. The guess is never displayed and never retrievable, so
     this is a narrower objection than the one against answer cards — but it is
     the same objection. Access is the plastic half by design; a generated
     paraphrase deciding what surfaces is not the kind of plasticity meant.
     Note the distinction from [Anticipation] above, which is not this: a `ctx`
     vector is measured from situations that actually happened, never written by
     a model.
     CUT: automatic answer cards, and answers stored as artifacts without the
     operator asking. A synthesised digest competing in search with the exact
     wording it was derived from is fidelity loss by design; the keep-this-answer
     link is the operator's decision, recorded as such, and that is the line.
     CUT: LLM excerpt compression at query time (extract the relevant sentences
     before answering). One more call to shave tokens off the next one; the
     cliff and the reranker do the same for free. -->

## [Retrieval]

What ask learns at write time, search inherits. Ask verdicts join judged
searches as access cues under **access reconsolidation** above. The cliff is
built (`search::cliff`, `src/core/search.rs:256`) and ask packs to it. The items
here are search's own, and every one of them moves ranking.

Built: **continues in**, and it turned out to be two things rather than one. On
the rail a hit says what its document does next — *continues in #2* where the
next passage itself placed, *continues in the next passage* where it did not,
never both. In the pane the document actually continues: opening a passage
appends the one after it, and a control appends the next as often as it is
clicked, until the document ends and the control becomes the way into the
document. The source column grows with the run, recomputed rather than
appended so the lines between two adjacent passages are not printed twice.

Two things it did not become. Not an inline expansion on the rail — the rail is
a `listbox` whose rows are options, and a disclosure inside an option breaks
the arrow keys as well as the ARIA. And not a semantic chain: adjacent passages
are additionally similar through their shared heading (see **Server-side
grouping** below), so such a chain ends where the formatting changes rather
than where the meaning does, and a pane whose length is a computed judgement
cannot tell the reader whether it stopped because the document ended or because
a number was not met. The run length rides in the link, so the reader is the
stopping rule. Cross-corpus was considered and belongs elsewhere: `ordinal` is
a per-corpus sequence, so there is no reading order to continue into, and what
a near artifact in another document is — a neighbour — the pane already shows
as *Related* and *Seen together*.

Built: **why this hit is where it is, as one object.** A rank is the product of
eight stages, and each used to say what it did in its own way or not at all.
One `HitExplanation` now rides on every ranked hit and one `SearchExplanation`
on the search, filled in as the pipeline walks, and all three doors read that
same object: the rail extends its `rail-why` line with the consequence in a
sentence, MCP's meta line lists the stages when the `explain` parameter asks,
and the API wraps its array as `{"results", "explanation"}` under `?explain=1`.
A door that did not ask sees byte-identical output to before — the flag gates
rendering and nothing else, and the order is the same either way.

What the estimate missed. Three of the eight stages run inside Qdrant, which
returns a fused score and no working. Rather than pay for a second query they
are reconstructed locally from payload fields the search already fetched
(`core::explain::scoring_terms`), and that reconstruction is pinned against a
real Qdrant in `tests/integration_qdrant.rs` — a unit test would only have
pinned our own belief about `exp_decay`, which is the thing in doubt. That
contract test is the branch's load-bearing one.

What it is not. Nothing is stored: there is no explanation table and no
migration, so the corpus-concentration figure this was built to measure has to
be gathered by deliberate searches rather than read off history — and each such
search writes, because `Door::Mcp` is captured and the `search` tool marks.
Keep the probes few. And `retrieved_rank` is named for what it is: the pre-
recency RRF rank is not obtainable without a second query, so the baseline is
what retrieval returned, fusion and scoring together, not fusion alone.

- **Server-side grouping — a prerequisite, not a nice-to-have.** The per-corpus
  cap is applied client-side over a candidate pool three times the limit; a
  corpus whose artifacts fill the pool leaves nothing to promote. At
  `synthesis = "off"` a 10,000-token document yields ~26 passages rather than ~8
  artifacts, adjacent passages are additionally similar through their shared
  heading, and one long document fills the pool reliably. The tiered-synthesis
  spec (§5) names Qdrant's `query/groups` as part of the design; what landed is
  the fallback only — `cap_per_corpus` now counts a merge against each of its
  origin corpora (`VectorPayload.origin_corpora`).
  **Worth:** without it, a single very long document at `off` can dominate a
  result list. This is a correctness ceiling on a shipped setting, not an
  improvement.
  **Cost:** the `query/groups` call in `vector/qdrant.rs`, its emulation in
  `vector/memory.rs`, and the judged-pair measurement. **A branch**, plus a
  harness run.
  **The instrument now exists.** Whether this is worth building or worth
  cutting is a number, and the ranking explanation above is what reads it:
  `CapEffect::Refilled` on a hit, and `displaced == refilled` on the pool line,
  are the cap redistributing nothing. Measure before committing the branch.

- **A dismissal that changes the next search.** Verdicts and dismissals are
  recorded and read — by gaps, by pursuits, by activation — and `verdict`
  appears in `src/core/search.rs` exactly once, in a comment. A hit sent away is
  therefore back tomorrow for the same question. As a negative example in a
  `best_score` recommend it is not: the penalty there is squared and
  sign-flipped, so it reaches the neighbours of the dismissed chunk and not only
  the chunk, where a payload exclusion reaches exactly one point.
  **Worth:** the one effect whose cause the operator set themselves. An effect
  on your own last action reads as a base that listened; the same effect with no
  visible cause reads as noise.
  **Cost:** negative examples threaded through the search path, sharing the
  `best_score` machinery with access reconsolidation — build the two in
  sequence, not apart. **A branch**, plus a harness run.

- **Reranking on by default**, once there is a default endpoint worth assuming.
  A cross-encoder, not a model call.
  **Worth:** likely the largest single-stage gain in ordinary retrieval, and it
  is the stage that makes a cliff form at all — which is why the ask questions
  above are downstream of it.
  **Cost:** a config default. The real cost is the dependency and both harnesses
  saying it earns it. **One commit**, plus two harness runs.

- **Spreading activation past one hop.** *(low, and put here saying so)*
  Association reaches exactly one hop from a hit. The general form is a
  diffusion — the cosine kNN graph unioned with the Hebbian link weights,
  personalised PageRank seeded by the hits above the cliff, run to convergence —
  which is what the phrase *spreading activation* means everywhere it is
  borrowed from, and it costs no model call at all.
  **Worth:** unproven, and there is reason to expect little. One hop from a
  strong hit is already the neighbour worth having; hops two and three are where
  a graph learned from co-display turns into a graph of what happened to be on
  the same page. It also runs against **Why this hit is where it is** above:
  *recalled through this link* is an explanation an operator can check, and
  *arrived at by diffusion over the whole graph* is not.
  **Cost:** a materialised kNN graph, which nothing keeps today — `neighbours`
  is per-request — plus its refresh on every capture, the walk itself, and a
  harness run to find out whether any of it beats the hop. **A project.** Last
  in this section, and it stays there until the hop it generalises has a number.

<!-- CUT: late-interaction reranking (ColBERT-style multivectors). Not for the
     reason first written here, which said storage and memory both. Memory is
     avoidable and the claim was wrong: a multivector kept out of HNSW
     (`hnsw_config: { m: 0 }`) is a rerank stage over the top-k rather than an
     index, and holds nothing resident. Storage is real at roughly the order
     first guessed, and bounded — a reduced-width vector per token against one
     dense vector per artifact, `on_disk`. Expensive, not ruinous. What stands
     is the last clause, and it is enough on its own: a model dependency, to
     beat a baseline that atomic, LLM-synthesised artifacts and hybrid search
     already make strong. A multivector of two to five *context* vectors per
     artifact is a different size of thing and is not covered by this cut. -->

## [What the base says about itself]

These are screens, and the opening of this file says nothing here is a screen to
look at. The exception is narrow and deliberate: these are instruments, not
reading material. Three defaults on this list — `[sitting] prime`, the block
weights, reranking — are held at their current values because nobody can see
their effect, and an instrument is what unblocks them. Anything on this page
that is not load-bearing in that sense has been cut.

Built: one queue for "the base did not answer" — four `GapKind`s on the list the
capture page already had, each badged with what asked it (*judged*, *asked*,
*nothing near*, *pursued*), closable by a capture that covers it, a pursuit that
earns itself, or dismissal, with coverage stored in `gap_coverage` so nothing an
automatic score decided overwrites what a person judged. And the offer's hit
rate on Ops, shown against clicked, by rung, over thirty days.

Built: **Housekeeping is Insights**, and it says what the memory is like rather
than only what needs sweeping. Three panels over tables that already existed —
how much is held and how much of it a model wrote, what is still reachable under
the decay a search would apply, and recall@10 and MRR read from
`feedback_stats` rather than recomputed beside it. No new table, no sweep, and
no model call, pinned by a test that counts embed calls across a page load.

An unjudged base says so instead of reporting `0.00`: that figure is the one
here whose absence must not look like a result. What is still missing is the
*series* — these are today's numbers, and **Learned block weights** wants
months of them.

- **Where an artifact came from, end to end.** Corpus lines, passage, the window
  whose reading earned a synthesis, the merge, the pursuit, the answer it was
  cited in. `store/lineage.rs` and `web/lineage_view.rs` hold the middle of this.
  **Worth:** the thesis of the whole application is that rewriting is *earned*,
  and an artifact cannot currently say what earned it or what it has ever
  answered. Real, but the weakest of the three: it is the one instrument here
  that unblocks no default.
  **Cost:** the two ends joined to the middle that exists, and a view.
  **A branch.** Last of this section.

## [Core Platform & Tooling]

Built: **one text surface for the whole web UI**, as the panel already had it
(spec `2026-08-22-one-text-surface-design.md`). Capture, search and ask were
three pages and are one route at `/ui`: one box that never changes shape, the
verb chosen by a button, and no state hidden between them. The three old doors
are deep links into it, so a bookmark, the extension's capture post and the
*keep this answer* flow all still land where they always did.

What this file left open there is answered. The rail belongs to the act — typing
fills it with results, **Ask** replaces them with the excerpts the answer was
written from, and one anchor goes back. The filter chips belong to the box,
because they qualify what typing does and nothing else. The judged-verdict bar
needed no decision at all: it was already under the answer, and it rode there
with it.

Two things the surface forced out of hiding and are worth keeping the reasons
for: pressing **Ask** disables the box, which *is* how search-while-type is
disabled — a disabled input fires no `keyup`, so there is no second mechanism to
keep in step — and every one of the three exits re-enables through the one
`stop()` the stream already funnelled them into, including the transport error
that would otherwise have left the page disabled for good.

Built: **multi-user tenancy**, and not in the shape this file predicted (spec
`2026-08-24-multi-user-tenancy-design.md`). It was listed here as
de-prioritised and payload-partitioned; it shipped as a database and a Qdrant
collection per user, with the job queue and one worker pool staying
instance-wide. The split follows what is actually scarce: files and collections
are cheap and get divided, the embed and synthesize endpoints are one GPU and
stay shared, and the queue is where the two planes meet, which is why the
tenant lives in the queue rather than in a registry beside it. Isolation is
structural — there is no tenant filter to forget, because no tenant filter
exists. Identity comes with it: the provider is the gate, and the local account
store engram used to keep for a single operator is gone.

Two items above are downstream of that. **Dropping the `scope` block** was
unblocked by it and has shipped. **OAuth 2.1 for `/mcp`** stopped
being administrative — with a collection per user, a bearer token is what
decides whose base a call reads.

Built: **the MCP door takes what the web doors take, and reads what they
read.** `ingest` took text and nothing else, which an agent noticed before this
file did. It takes exactly one of text, a link or a file as base64 now, with a
note beside it; the link goes through the same `Core::ingest_url` the
paste-a-link door does, so a URL to a PDF or an image is stored for its reading
stage at either door and the link is provenance on the corpus. A file handed
over as bytes is known by its bytes, not its name — a PDF by its header, an
image by its own, otherwise UTF-8 text or a refusal by name. What was
deliberately not added: a path on this server's disk, because with a base per
user the server's disk is nobody's. Two things beside it. `read` returns the
document a hit was cut from, verbatim and in pages, by the corpus or artifact
id search prints — search over MCP was a dead end before it, and the paragraph
after the one that matched is often the answer. And the meta line now says in
words everything the rail says with a badge or a shade of grey: weak, written
or synthesized by a model and from how many sources, superseded and by what,
deprecated, lifted by priming or by the sitting. A model-written text read as a
captured one over MCP until then, which is the one thing that field forbids.

Built: **a phone browser is offered the app.** Registering the worker made
engram installable and nothing asked. On a phone browser that is not already
the installed window a toast above the tab bar offers it, once a week —
Chrome's own prompt where the browser hands one over, the Share-sheet route
on Safari, nothing at all on a browser with neither. Never on a desktop and
never in an installed window.

- **One dial instead of eight gates.** Three are gone: `[learn]` is now the
  single switch over recording, association and pursuits, and the sections below
  it keep their thresholds and no switch of their own. What is left is the half
  where the flags are genuinely different questions: `[activation]`,
  `[promote]` and `[consolidate]` still depend on each other in ways only the
  config comments admit — promotion reads an activation that only moves while
  `[learn]` is on, and priming exists only to be fed by activation.
  **Worth:** a named mode — off, learning, full — setting a coherent bundle, with
  the individual keys still there for whoever wants them. Every combination
  currently refused at startup is a setting that was written more than once.
  **Cost:** a mode in `config.rs` resolving to the existing keys, and the
  startup refusals reduced to the ones that remain possible. **One commit.**

- **Structure in a PDF.** The default `pdf-text` build recovers words and
  reading order and *no* headings or tables — measured, and pinned by a test that
  fails if that improves. The splitter falls back to blank lines, so every window
  loses the heading it would have carried. `--features pdf-ml` adds the layout
  and table models, at the price of the ONNX runtime, pdfium as a native library
  and a model download; a scan is refused with that reason until it is switched
  on.
  **Worth:** every window in a PDF currently loses its heading, which is the one
  piece of context synthesis most wants. Real and measurable.
  **Cost:** none in code — it is built behind a feature flag. The cost is the
  dependency, and making it the default waits on someone wanting it enough to
  pay for it. **One commit**, whenever that is true.

- **Images in a source, shown where the source is read.** The text itself is
  clean now: `core::pdf::normalise` folds a detached bullet glyph — a symbol
  font's private-use one or a real U+2022 — into a markdown list marker and
  collapses blank-line runs, pinned by `tests/fixtures/bullet-list.pdf`. A PDF's
  figures are dropped by extraction, and the corpus page has no place for them
  anyway: it shows the photo of an image capture and the text of everything else.
  **Worth:** a figure is often the answer, and today it is not in the base at
  all. Bounded, though — it changes reading, not retrieval.
  **Cost:** three things that do not exist: the images pulled out of the PDF and
  stored (attachments are one row per corpus today, and this is many), a span or
  anchor tying each to the place in the markdown it came from, and a renderer.
  **A project.**

- **A CLI.** There is no door to the base from a shell that is not `curl`.
  **Worth:** low while `/mcp` exists and is the door an agent already uses.
  On the list because scripted capture and export are the two things a shell is
  genuinely better at.
  **Cost:** a binary over the existing API. **A branch.** Near the bottom.

- **DOCX, EPUB, XLSX and the rest.** `docling` is in the tree and already reads
  them; only a door and a `kind` are missing. Deliberately out of PDF capture.
  **Worth:** whole classes of source that currently cannot be captured at all,
  for almost no new machinery.
  **Cost:** a `kind`, a door, and the extraction path pointed at it.
  **One commit** per format after the first, **a branch** for the first.

- **Backup and restore.** Qdrant snapshots plus the SQLite file, restored
  together.
  **Worth:** recovery that does not mean paying for every embedding again. The
  only item on this list whose absence can cost real money.
  **Cost:** a snapshot call, a file copy, a restore path that checks the two
  agree. **A branch.**

- **OAuth 2.1 for `/mcp`.** The web UI's identity is the provider's now, and
  the local one it used to keep is gone. `/mcp` still authenticates on its own
  terms: a bearer token, checked against `api_tokens`, with nothing but the
  protected-resource host taken from the OIDC config.
  **Worth:** one identity model instead of two, and it is no longer only
  administrative. With a database and a collection per user, a token is what
  decides *whose* base a call reads — an answer the web door now gets from the
  provider and the MCP door still gets from a row.
  **Cost:** the MCP surface moved onto the existing OIDC path. **A branch.**

- `clippy` is not run locally in every environment; CI is the only gate.

<!-- CUT: quantization and on-disk payload for small hosts. Hydrating artifacts
     from SQLite makes every search cross a layer it does not need to, to save
     memory nobody has run out of. Text stays in the Qdrant payload. -->
