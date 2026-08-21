# Roadmap

Not built yet, roughly in the order it would be worth building.

engram is a memory, and from here on it is designed as an **expansion of a
biological one**. It keeps the one capability the brain lacks — verbatim recall
with provenance — and borrows the brain's mechanisms for everything that decides
how a memory is reached: association, activation, priming, forgetting, sleep.
The search box stays the application. Nothing here is a screen to look at; it
is what makes the answer to the situation you are in come first, while you are
still typing it.

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

Most of what is left is not a mechanism but a seam. Three of them are now
closed — the queue is the scheduler, the sitting is live, and the four ways of
saying *the base did not answer* end on one list
(`docs/superpowers/specs/2026-08-20-one-system-design.md`). What is left below
is what those three make easier rather than what they replace. The list is
grouped by subject, as it always was, but read end to end it is one project —
and roughly in the order it would be built, because a seam that only joins two
surfaces ships when it is written, while a seam that moves ranking waits for
the harness.

## [Associative Memory]

Spec: `docs/superpowers/specs/2026-08-16-associative-memory-design.md` — built:
Hebbian links learned from co-retrieval, decaying activation per artifact,
bounded priming and one-hop association in the results, a sparse judge on
strong cross-corpus links, switched on with `[associate]` and `[activation]`
in config. The items below are the mechanisms that come after it, in order.

- **Sleep as an explicit cycle.** Built, and not as a cycle. The queue was
  already three-quarters of a scheduler; it has a priority column now
  (`jobs.class` — is somebody waiting on this?), ageing on the repair pass, and
  five fewer tickers: a sweep arms itself one interval out when it finishes, so
  `run_after` is the cursor saying when it last ran. Ordering is expressed the
  way the tree already expressed dependencies, by arming — the association
  sweep pulls the pursuit sweep forward, replay before pursue. The account is
  `sweep_runs`, one row per run, shown on Ops as the last day with the history
  under it.

  What did not survive contact is the *cycle*. Units on their own periods do
  not line up into one night, and grouping them by an invented cycle identity
  would be inventing it — so there is no "last sleep", there is the last day.
  Repair stays outside the schedule: it is what recovers an interrupted one.
- **Working memory.** Built, carrying only. A live sitting keyed by web
  session (`src/core/sitting.rs`), in memory, expiring at `pursuit.idle_secs` —
  the same number the sweep uses, so the live definition and the reconstructed
  one agree by construction. It joins the doors: the query carries from search
  into ask, an answer kept from ask shows the question it answered, and both
  pages carry a rail of what this sitting has been in. It never writes
  activation, and there is a test saying so.

  Priming from it is behind `[sitting] prime`, off, sharing the one budget
  `associate.prime_lift` bounds. It is the one thing here that moves ranking,
  so it waits for the harness — see below.
- **`[sitting] prime` is unmeasured.** The one default here that would move
  ranking, and the harness has not been run either way: it needs a live Qdrant,
  a real embedding endpoint and a corpus that is not in this repository. Until
  it has, the flag stays `false`. It moves in a commit of its own, carrying
  both numbers in its message, or it does not move.
- **Every door counts as engagement.** Retrieval is recorded at every door —
  `core.search` bumps activation whether the query came from the rail, the API
  or `/mcp`. Engagement is not: `mark_artifact_seen` and `record_interaction`
  have exactly one production caller between them, the dwell route at
  `src/web/ui.rs:2909`. So an artifact a Claude Code session read all afternoon
  is never opened, never promoted and never part of a pursuit, and the base's
  picture of what is used is a picture of the web UI only. A sitting is a
  sitting whichever door it came through.
- **Access reconsolidation.** A judged hit says "for this query, that artifact".
  The query becomes an additional access cue for the artifact — a second
  vector or a stored cue list — so the next similar situation finds it
  directly. Ask verdicts are the second source and the better one: a carried
  excerpt says the same thing about a question a person asked in earnest. Text
  untouched; changes what vectors are built from, so it waits for the harness
  to say it helps.

  *There is a cheaper shape of it that does not wait on re-embedding.* Qdrant's
  `recommend` with `strategy: "best_score"` carries the cue as a second
  positive example at query time: the stored vector is left alone, and a
  candidate is scored by `max` over the examples rather than by their mean, so
  the query and the remembered question stay two independent ways in instead of
  collapsing to a midpoint that is neither. `average_vector` is the wrong knob
  here for exactly that reason. It still moves ranking, so it is still the
  harness's call; what it changes is the cost of being wrong, from a
  re-embedding pass to a flag.
- **A forgotten list with a direction.** `resurface` samples at random —
  `{"sample": "random"}` in `vector/qdrant.rs` under payload filters — old,
  unseen, otherwise arbitrary, which is a list you read once. Qdrant's context
  search takes pairs where a query would go and scores a candidate by which
  side of each pair it falls on, so the sitting's rail is the positive side and
  `superseded`/`deprecated` the negative one, and what comes back is old and
  unseen *and* near what this sitting has been in. The property that makes
  context search a poor search — everything inside the admitted zone ties, so
  the order within it is arbitrary — is the right property here: a forgotten
  list wants spread within a subject, not the same five every time. Alone among
  the three Qdrant items on this list it moves no ranking, because the
  forgotten list is its own list. It needs no harness and can go first.
- **Error-driven re-synthesis.** An artifact shown often and never confirmed,
  or judged noise, is misleading — a title that over-claims, a passage that
  lost its context. It is re-synthesised **from its source segment**, never from
  itself, with before/after on Ops. The exposure and confirmation counts the
  activation work provides are the detector.
- **Usage-informed supersede.** `auto_supersede` keeps the newest member of a
  near-identical group. Activation knows which member people actually
  confirmed. First shown on the undo list; changed only if it turns out to
  matter.
- **Corpus map.** The distance-matrix API over a filtered subset, plus the link
  table, drawn. Nice to look at, not a way to use the app; last.

<!-- NOT COPIED from the brain, on purpose: confabulation (no answer cards, no
     generated answers standing in for stored text — a synthesised digest
     competes with the exact wording it was derived from, which is fidelity
     loss by design), content decay (activation fades, artifacts do not),
     interference (a new capture never overwrites an old one; a conflict goes
     to a person). These are where the expansion is deliberately better than
     the thing it expands. -->

## [Ask]

Ask is the part of engram that is allowed to think, and it is built (spec
`2026-08-17-streaming-ask-design.md`). It streams to the page — reasoning
tokens included, when the model emits them — packs excerpts to the relevance
cliff rather than to the context window, reaches one hop sideways for
candidates, checks its own literals against the excerpts it was shown, and
offers the answer back as a paste the operator approves. `[infer.ask] plan`
adds one bounded round of planned retrieval and ships **on**: after the first
round the model names the subjects the excerpts miss, each becomes a search of
its own, and up to three run at once and merge into one set of excerpts. One
plan, never a second, and never a loop.

Two things it did not give up. An answer cannot carry a literal the excerpts
did not — `verify::missing_literals`, the same guard synthesis runs, now
applied to generation. And nothing is written to memory without a person: the
keep-this-answer link prefills the capture box and saves nothing, so the trace
records that a model wrote the text and what it was written from.

Model tiers are built. The config carries named tiers under `[infer.tiers.*]`
and each chat role points at one, resolved at parse time into the same concrete
role structs the completers already took — so nothing downstream changed. The
inline shape still parses and warns, naming its replacement. The capability the
rename existed to express is now used: the planning call runs on the efficient
tier while the answer it feeds runs on the deep one.

The ask harness and ask feedback are built (spec
`2026-08-17-ask-harness-design.md`): verdicts on the answer page with carriers,
`questions.json` in the export, `evaluate_ask` measuring citation recall,
abstention accuracy and faithfulness by literals and by claim check, plus an
unsupported-literal count, and "nothing here" surfaced as knowledge gaps on the
capture page.

What is left here is one number nobody has run yet: whether the planning call
earns itself, and whether packing to the cliff helps on a base with no reranker
configured. A search's own fused scores are smooth enough that a cliff may
rarely form without one — the rail has always lived with that, and ask now
inherits it. Both are harness questions, not design ones.

What ask learns about retrieval it still mostly keeps. Three edges are missing,
and none of them costs a call:

- **Co-citation is a stronger link than co-display.** `associate` learns its
  Hebbian links by replaying the search log: two artifacts shown in one result
  list are drawn together. Two artifacts the model *cited in one answer* were
  used together, which is the same claim with the noise taken out. The
  citations are already stored with the ask.
- **A citation is an engagement.** It bumps nothing. This is the same hole the
  pursuit sweep's promotion call papers over under **Core Platform** below,
  seen from the other end — and giving the citation its own bump is the fix
  that entry names, which retires the sweep's call as a side effect.
- **The plan's uncovered subjects are gap candidates.** When `[infer.ask] plan`
  names the subjects the excerpts miss, it has said out loud what the base does
  not hold, one bounded round per question, in the model's own words. Today
  each becomes a search and is then thrown away. A subject whose fan-out search
  came back with nothing is a knowledge gap that cost nothing to find.

<!-- CUT: situation vectors — at ingest, the model writing the three to five
     situations an artifact answers, each embedded as an extra named vector, so
     a typed situation matches a question rather than a passage. Cut on the
     fidelity line: it puts a model's guess about what an artifact answers into
     the ranking path. The guess is never displayed and never retrievable, so
     this is a narrower objection than the one against answer cards — but it is
     the same objection. Access is the plastic half by design; a generated
     paraphrase deciding what surfaces is not the kind of plasticity meant.
     CUT: automatic answer cards, and answers stored as artifacts without the
     operator asking. A synthesised digest competing in search with the exact
     wording it was derived from is fidelity loss by design; the keep-this-answer
     link is the operator's decision, recorded as such, and that is the line.
     CUT: LLM excerpt compression at query time (extract the relevant
     sentences before answering). One more call to shave tokens off the next
     one; the cliff and the reranker do the same for free. -->

## [Retrieval]

What ask learns at write time, search inherits. Ask verdicts join judged
searches as access cues under **access reconsolidation** above. The cliff is
built (`search::cliff`) and ask now packs to it. The items here are search's
own.

- **Continues in.** A hit whose neighbour in its corpus (adjacent ordinal) is
  also above the cliff says so, and one click reads on. The answer to a
  situation is often the paragraph after the one that matched. *Most of the
  machinery arrived with ask's sideways reach:* `Store::adjacent_artifacts`
  exists, and `ask/retrieve.rs` already decides which hits are reliable enough
  to reach from. What is left is the presentation — saying so on the rail, and
  the click.
- **Server-side grouping — now a prerequisite, not a nice-to-have.** The
  per-corpus cap is applied client-side over a candidate pool three times the
  limit; a corpus whose artifacts fill the pool leaves nothing to promote. At
  `synthesis = "off"` a 10,000-token document yields ~26 passages rather than
  ~8 artifacts, adjacent passages are additionally similar through their
  shared heading, and one long document fills the pool reliably. The
  tiered-synthesis spec (§5, "What `off` makes mandatory") names Qdrant's
  `query/groups` as part of the design; what landed is the fallback only —
  `cap_per_corpus` in `src/core/search.rs` now counts a merge against each of
  its origin corpora (`VectorPayload.origin_corpora`). Still to build: the
  `query/groups` call in `vector/qdrant.rs`, its emulation in
  `vector/memory.rs`, and the measurement against the judged-pair set, since
  it moves ranking. Until then a very long document at `off` can dominate a
  result list.
- **A dismissal that changes the next search.** Verdicts and dismissals are
  recorded and read — by gaps, by pursuits, by activation — and `verdict`
  appears in `src/core/search.rs` exactly once, in a comment. A hit sent away
  is therefore back tomorrow for the same question. As a negative example in a
  `best_score` recommend it is not: the penalty there is squared and
  sign-flipped, so it reaches the neighbours of the dismissed chunk and not
  only the chunk, where a payload exclusion reaches exactly one point. This is
  the one whose cause the operator set themselves — an effect on your own last
  action reads as a base that listened, where the same effect with no visible
  cause reads as noise. It moves ranking, and it is worth the measurement it
  costs.
- **Reranking on by default**, once there is a default endpoint worth assuming.
  A cross-encoder, not a model call; the harness — both of them — decides.
- **Why this hit is where it is, as one object.** A rank is now the product of
  hybrid fusion, the recency stage, the pinned boost, the reranker,
  `cap_per_corpus`, `prime`, the cliff and the one-hop reach — eight stages
  layered in the order they were built, each saying what it did in its own way
  or not at all. The rail badges some of it, MCP's meta line badges a different
  some of it, the API a third. One explanation carried on the hit — lifted two
  places on activation, recalled through this link, past the cliff — is what
  lets all three doors say the same thing, and it is the only honest way to
  keep adding stages to a ranking the operator is asked to trust.

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

The opening of this file says nothing here is a screen to look at, and that
holds for the mechanisms. It does not hold for their results. Three things the
base already knows are currently spread across four pages in four vocabularies,
and a mechanism the operator cannot see the effect of is one they cannot decide
to trust.

- **One queue for "the base did not answer".** Built. Four `GapKind`s on the
  one list the capture page already had, each badged with what asked it:
  *judged*, *asked*, *nothing near*, *pursued*. Closable three ways — a capture
  that covers it, a pursuit that earns itself, or dismissal — and a capture
  that answered something says so on its own queue row, which is the loop
  shutting where the operator can see it.

  The fourth source is distance, not behaviour: a search whose best candidate
  fell under `vector.weak_below`. *Abandoned* was the first draft and it was
  wrong twice — not clicking a result can mean the list was useless or that the
  titles told you what you needed, and an open is only recorded when pursuits
  *and* feedback are on, so with pursuits off every search looks abandoned.
  Coverage is stored in `gap_coverage` rather than on the judged row, so
  nothing an automatic score decided overwrites what a person judged, and
  deleting the capture that closed a gap reopens it.
- **Ops as the state of the memory, not the housekeeping table.** It lists
  merges, deprecations, hidden near-duplicates, retries. What it does not say
  is what the memory is *like*: how much is held and how densely, what is
  activated and what is fading, recall@10 and MRR over months rather than as
  today's number on the judge page. Four figures in four places, and the one
  page named for the answer has none of them. What the sweeps did is answered
  now — the last day, and the history under it — which leaves the shape of the
  base itself.
- **Where an artifact came from, end to end.** Corpus lines, passage, the
  window whose reading earned a synthesis, the merge, the pursuit, the answer
  it was cited in. `store/lineage.rs` and `web/lineage_view.rs` hold the middle
  of this. The thesis of the whole application is that rewriting is *earned* —
  and an artifact cannot currently tell the operator what earned it, or what it
  has ever answered. Last of the three, and the one that makes the other two
  worth reading.

- **The offer's hit rate is on Ops.** Built. Shown against clicked, by rung,
  over the last thirty days. The block weights the recommendation rests on are
  chosen, not measured, and this is the instrument that would let them be
  fitted — fitting them before the data exists would be guessing with extra
  steps. It is here for the reason `[sitting] prime` is still `false`: a
  default nobody can see the effect of never moves.

- **Dropping the `scope` block.** Not built, and it waits on per-user
  collections rather than on anyone's judgement. At weight 10 against a total
  under 5, that block is what keeps one person's situations from being ranked
  first for another; the read path cuts foreign clusters exactly, so the block
  is a ranking aid rather than the guarantee, but it stays until each user has
  their own collection. Then it goes to 0 and nothing else about the encoder
  changes.

- **Learned block weights.** Not built. Once the shown/clicked rate has months
  behind it, the weights in `[recommend.weights]` can be fitted to it. Until
  then they are the defaults in the design record, and the honest description
  of them is "chosen".

- **Conjunctions across scopes.** Not built. The context vector can hold
  "on the phone the hour matters, at the desk it does not"; nothing yet learns
  which of those conjunctions are real.

## [Core Platform & Tooling]

- **One text surface for the whole web UI, as the panel now has.** Capture,
  search and ask are three pages, and moving between them means retyping or
  carrying a prefill: the same words are a query on one, a question on the
  second and a document on the third, and the operator navigates to say which.
  The extension's panel does not. It is one box that never changes shape, with
  the verb chosen by a button — typing searches, **Ask** spends the model call,
  **Capture** stores what is in the box — and no state hidden between them.
  That is the thesis at the top of this file made literal: the box is the
  application, and the page you are on stops being a thing to decide. It ships
  in the extension first because a side panel is 350 pixels of one column,
  which is the cheapest possible place to find out whether one surface really
  does hold three verbs without any of them getting in the way. If it does, the
  three pages fold into `/ui/search` and the others become deep links to it.
  What has to be answered there and not here: where the rail, the filter chips
  and the judged-verdict bar live when the box is doing all three jobs.
- **One dial instead of eight gates.** Three of them are gone: `[learn]` is now
  the single switch over recording, association and pursuits, and the sections
  below it keep their thresholds and no switch of their own. That was the half
  of this item where the flags were not really independent — two of their
  combinations were refused at startup and a third was a warning, which is how
  you find out a setting has been written three times. What is left is the half
  where they are genuinely different questions: `[activation]`, `[promote]` and
  `[consolidate]` still depend on each other in ways only the config comments
  admit — promotion reads an activation that only moves while `[learn]` is on,
  and priming exists only to be fed by activation. A named mode — off,
  learning, full — setting a coherent bundle across those, with the individual
  keys still there for whoever wants them, is what would finish it.
- **A CLI.** PDF capture is built: `docling` reads an uploaded PDF into markdown
  in `Stage::Extract`, locally and without a model, and the corpus is text like
  any other from there. Spans into it are line spans labelled `extraction`, not
  `page 42` — a page map is a second coordinate system beside every stored span,
  and the label was not worth it.
- **Structure in a PDF.** The default `pdf-text` build recovers words and
  reading order and *no* headings or tables — measured, and pinned by a test
  that fails if that improves. The splitter falls back to blank lines, so every
  window loses the heading it would have carried. `--features pdf-ml` adds the
  layout and table models, at the price of the ONNX runtime, pdfium as a native
  library and a model download; a scan is refused with that reason until it is
  switched on. Making it the default waits on someone wanting it enough to pay
  for it.
- **Images in a source, shown where the source is read.** *(The text itself is
  clean now: `core::pdf::normalise` folds a detached bullet glyph — a symbol
  font's private-use one or a real U+2022 — into a markdown list marker and
  collapses blank-line runs, leaving indentation and every glyph that is not
  standing in for a marker where they are, pinned by
  `tests/fixtures/bullet-list.pdf`.)* A PDF's figures are dropped by extraction,
  and the corpus page has no place for them anyway: it shows the photo of an
  image capture and the text of everything else. Showing
  a document's figures inline — on the raw corpus and on the passages that
  claim their lines — needs three things that do not exist: the images pulled
  out of the PDF and stored (attachments are one row per corpus today, and this
  is many), a span or anchor tying each to the place in the markdown it came
  from, and a renderer for it. Worth doing after the text itself is clean.
- **The pursuit sweep's promotion call, which almost never has anything to do.**
  A one-engaged-artifact pursuit calls `maybe_promote`, but every engagement
  already calls it at the bump — `search::mark_artifact_seen` on an open,
  `associate` on a confirmation — and the sweep only runs once the sitting has
  been idle for `idle_secs`. It therefore re-checks the same artifact against a
  *more* decayed activation than the live call saw, and can only ever decline
  where that one declined. The one case it does cover: an artifact engaged
  solely by an ask citation, which counts toward the pursuit but bumps no
  activation. Either give a citation its own bump and drop the call, or leave
  it as the cheap backstop it is.
- **DOCX, EPUB, XLSX and the rest.** `docling` is in the tree and already reads
  them; only a door and a `kind` are missing. Deliberately out of PDF capture.
- **Backup and restore.** Qdrant snapshots plus the SQLite file, restored
  together, so recovery does not mean paying for every embedding again.
- **OAuth 2.1 for `/mcp`.** OIDC login for the web UI is built; the MCP surface
  still authenticates on its own terms.
- **Multi-user tenancy.** *(de-prioritised)* Payload-partitioned rather than a
  collection per user. Single-operator use is the design point.
- `clippy` is not run locally in every environment; CI is the only gate.

<!-- CUT: quantization and on-disk payload for small hosts. Hydrating artifacts
     from SQLite makes every search cross a layer it does not need to, to save
     memory nobody has run out of. Text stays in the Qdrant payload. -->
