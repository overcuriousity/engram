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
claim check; knowledge gaps grouped and named on the capture page; a
recommendation under the search box, learned from the situations an artifact
was opened in — the browser's time zone and local time, the device, the
viewport, the network and the power state, clustered per artifact and stored as
a `ctx` multivector scored with `max_sim`, with the blocks that decided each
offer named beneath it and shown-against-clicked broken down by rung on Ops. A
situation seen once or twice is offered too, saying so in words — "Twice
before" — and held to a stricter match than an established one; with nothing
learned about the situation a card is drawn at random and claims nothing.
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

Everything plastic in the base is downstream of one question, and it has one
answer today: use means the web UI. `jobs::context` clusters `interactions`
rows into the `ctx` vectors the recommendation rests on; `associate` replays the
search log; `promote` reads an activation that only moves at a bump. All three
are fed through a single production caller, and the first two items below are
about widening that. Nothing else on this list is worth as much per line
changed.

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

- **Every door counts as engagement.** Retrieval is recorded everywhere:
  `core::search` bumps activation whether the query came from the rail, the API
  or `/mcp`, and `Origin` (`src/store/feedback.rs:77`) already carries the door,
  the subject and — for the web — the session. Engagement is not.
  `mark_artifact_seen` and `record_interaction` have exactly one production
  caller between them, the dwell route at `src/web/ui.rs:3766`. An artifact a
  Claude Code session read all afternoon is never opened, never promoted, never
  part of a pursuit, and never a situation anybody learns from.

  The doors are not symmetrical, and that is where the actual work is.
  `GET /api/v1/artifacts/{id}` is an open and can say so in one line. `/mcp` has
  no open at all: its tools are search, ask and ingest, and a search returns the
  whole artifact, so the read *is* the result. Counting every returned artifact
  as engagement would only relearn what association already learns from display.
  The honest signal at that door is the citation, which is why the next item is
  half of this one.

  **Worth:** the anticipating half of the application stops being blind at two
  doors out of three. Today an operator who works through `/mcp` teaches the
  offer ladder, promotion and pursuits precisely nothing, and the base's picture
  of what is used is a picture of the web UI.
  **Cost:** one line in the API artifact route, the citation bump below, and a
  recorded decision about what MCP counts as use. **One commit.** No harness: it
  changes what is learned, not how a fixed input is ranked, and no corpus in
  this repository contains real multi-door use anyway.

- **A citation is an engagement.** `ask_citations` already stores what the model
  was shown and what it used (`src/store/asks.rs:153`, column `used`). An
  artifact the model cited was used, and it bumps nothing.
  **Worth:** closes the `/mcp` half of the item above with the one signal that
  door honestly has, and retires the pursuit sweep's promotion call under Core
  Platform — an ask-cited artifact is the only case that call still covers.
  **Cost:** one call where the ask is recorded, minus `maybe_promote` in
  `jobs/pursuit.rs` and the test pinning it. **One commit.**

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

- **Error-driven re-synthesis.** An artifact shown often and never confirmed, or
  judged noise, is misleading — a title that over-claims, a passage that lost its
  context. It is re-synthesised **from its source segment**, never from itself,
  with before/after on Ops.
  **Worth:** the artifacts that mislead get repaired instead of accumulating,
  and the detector is free — the exposure and confirmation counts activation
  already keeps.
  **Cost:** a job beside `jobs/synthesize.rs`, re-synthesis from the segment,
  before/after on Ops, an undo path. **A branch.** No harness — it changes text
  through the guards that already exist, not ranking.

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
thirty days, on Ops. Everything left here is gated on that instrument having
months behind it, not on anybody's judgement.

- **Learned block weights.** The weights in `[recommend.weights]` are chosen,
  not measured, and the honest description of them is "chosen". Once the
  shown/clicked rate has history, they can be fitted to it.
  **Worth:** the offer stops being a guess with a good story behind it. It is
  also the precondition for the item below.
  **Cost:** a fit over `offer_rates` history and the restraint to leave the
  defaults alone until the history exists. **A branch**, gated on data rather
  than on code. Fitting them before the data exists is guessing with extra
  steps.

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

- **Dropping the `scope` block.** At weight 10 against a total under 5, that
  block is what keeps one person's situations from being ranked first for
  another. The read path cuts foreign clusters exactly, so the block is a
  ranking aid rather than the guarantee — but it stays until each user has their
  own collection.
  **Worth:** none for a single operator. It is a correctness item for the day
  tenancy exists, and nothing else about the encoder changes with it.
  **Cost:** one weight to 0. **One commit**, blocked on **Multi-user tenancy**
  below.

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

Model tiers are built: named tiers under `[infer.tiers.*]`, each chat role
pointing at one, resolved at parse time into the same concrete role structs the
completers already took. The planning call runs on the efficient tier while the
answer it feeds runs on the deep one. The ask harness and ask feedback are built
too (`2026-08-17-ask-harness-design.md`): verdicts with carriers,
`questions.json` in the export, `evaluate_ask` measuring citation recall,
abstention accuracy and faithfulness by literals and by claim check, and
"nothing here" surfaced as a knowledge gap.

- **The plan's uncovered subjects are gap candidates.** When `[infer.ask] plan`
  names the subjects the excerpts miss, it has said out loud what the base does
  not hold, in the model's own words, one bounded round per question. Today each
  becomes a search and is then thrown away. A subject whose fan-out search came
  back with nothing is a knowledge gap that cost nothing to find.
  **Worth:** gaps from a call already paid for, and the most specific ones on
  the queue — a named subject beats "this search scored low".
  **Cost:** keep what the plan already computed and write it as a fifth
  `GapKind` (`src/store/gaps.rs:10`), with the same coverage and dismissal paths
  the other four use. **One commit.**

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
here are search's own, and all but the first move ranking.

- **Continues in.** A hit whose neighbour in its corpus (adjacent ordinal) is
  also above the cliff says so, and one click reads on. The answer to a
  situation is often the paragraph after the one that matched. Most of the
  machinery arrived with ask's sideways reach: `Store::adjacent_artifacts`
  exists (`src/store/artifacts.rs:660`), and `ask/retrieve.rs` already decides
  which hits are reliable enough to reach from. What is left is the presentation
  — saying so on the rail, and the click.
  **Worth:** the paragraph after the match stops being something the operator
  has to go and find. Highest worth-per-line in this section, and the only item
  here that needs no measurement.
  **Cost:** a badge on the rail and a route behind the click. **One commit.**

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

- **Why this hit is where it is, as one object.** A rank is now the product of
  hybrid fusion, the recency stage, the pinned boost, the reranker,
  `cap_per_corpus`, `prime`, the cliff and the one-hop reach — eight stages
  layered in the order they were built, each saying what it did in its own way
  or not at all. The rail badges some of it, MCP's meta line a different some,
  the API a third.
  **Worth:** one explanation carried on the hit — lifted two places on
  activation, recalled through this link, past the cliff — is what lets all three
  doors say the same thing. It is the only honest way to keep adding stages to a
  ranking the operator is asked to trust, and every ranking item above adds one.
  **Cost:** an explanation struct threaded through eight stages in
  `src/core/search.rs`, then read by the rail, MCP's meta line and the API.
  **A branch.** No harness — it changes nothing about the order.

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

- **The pursuit sweep's promotion call, which almost never has anything to do.**
  A one-engaged-artifact pursuit calls `maybe_promote`, but every engagement
  already calls it at the bump, and the sweep only runs once the sitting has been
  idle for `idle_secs` — so it re-checks the same artifact against a *more*
  decayed activation than the live call saw, and can only ever decline where that
  one declined. The single case it covers is an artifact engaged solely by an ask
  citation.
  **Worth:** one fewer call that cannot do anything, and one fewer thing to
  explain.
  **Cost:** free — **A citation is an engagement** above removes the last case
  and this deletes itself with it. Not a separate item so much as a consequence.

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

- **OAuth 2.1 for `/mcp`.** OIDC login for the web UI is built; the MCP surface
  still authenticates on its own terms.
  **Worth:** one identity model instead of two — and `/mcp` is about to become
  the door that teaches the base, which makes "who is this" load-bearing rather
  than administrative.
  **Cost:** the MCP surface moved onto the existing OIDC path. **A branch.**

- **Multi-user tenancy.** *(de-prioritised)* Payload-partitioned rather than a
  collection per user. Single-operator use is the design point.
  **Worth:** none for the design point. It unblocks **Dropping the `scope`
  block** and nothing else.
  **Cost:** **a project**, and it stays de-prioritised.

- `clippy` is not run locally in every environment; CI is the only gate.

<!-- CUT: quantization and on-disk payload for small hosts. Hydrating artifacts
     from SQLite makes every search cross a layer it does not need to, to save
     memory nobody has run out of. Text stays in the Qdrant payload. -->
