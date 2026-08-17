# Roadmap

Not built yet, roughly in the order it would be worth building.

engram is a memory, and from here on it is designed as an **expansion of a
biological one**. It keeps the one capability the brain lacks — verbatim recall
with provenance — and borrows the brain's mechanisms for everything that decides
how a memory is reached: association, activation, priming, forgetting, sleep.
The search box stays the application. Nothing here is a screen to look at; it
is what makes the answer to the situation you are in come first, while you are
still typing it.

What is built: the pipeline from capture to ranked artifact; the last hop from a
ranked artifact back to its corpus lines; synthesis verified against what it
must not alter; filter chips from facet counts; nearest neighbours in the
detail pane; near-duplicate detection at capture; autonomous consolidation with
complete pair coverage, merge-loss checks and undo; caveats on artifacts; text
and image capture; hybrid search inside Qdrant; the evaluation harness fed from
judged real searches (`cargo test --test eval`, `/ui/judge`, `--export-eval`);
Hebbian links learned from co-retrieval with bounded priming and one-hop
association in the results; the cliff — where a ranked list's relevance falls
off — marked on the rail, over the API and over MCP; a single-shot ask endpoint
— retrieve, pack by score, one completion, cite by number, dropped excerpts and
truncation reported; judged questions with a second harness — citation recall,
abstention, faithfulness by literals and by claim check; knowledge gaps grouped
and named on the capture page.
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

## [Associative Memory]

Spec: `docs/superpowers/specs/2026-08-16-associative-memory-design.md` — built:
Hebbian links learned from co-retrieval, decaying activation per artifact,
bounded priming and one-hop association in the results, a sparse judge on
strong cross-corpus links, switched on with `[associate]` and `[activation]`
in config. The items below are the mechanisms that come after it, in order.

- **Sleep as an explicit cycle.** The background work already exists — link
  replay, activation decay, pruning, relate/dedupe, retention. It becomes phases
  of one scheduled cycle with one Ops report: last sleep, what changed. Framing
  over what exists, and the home every later mechanism slots into.
- **Working memory.** Within a session, what you just opened primes its
  neighbours and links for the next query. Session-scoped, expires with it,
  never written to activation.
- **Access reconsolidation.** A judged hit says "for this query, that artifact".
  The query becomes an additional access cue for the artifact — a second
  vector or a stored cue list — so the next similar situation finds it
  directly. Text untouched; changes what vectors are built from, so it waits
  for the harness to say it helps.
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

Ask today is the 2023 baseline: one embedding, top eight, greedy pack, one
completion. Nothing about it is measured, nothing it does feeds back, and the
page shows a spinner and then a wall of text. The pieces below turn it into the
part of engram that is allowed to think, without giving up the two things that
make it engram's: an answer cannot carry a literal the excerpts did not, and a
default moves only after the harness has run. Order matters — each item is
built on the one before it — and the split is such that every write-time piece
lands in [Retrieval] as well.

Model tiers: ask already runs on a larger, thinking model than synthesis does.
The config grows two named tiers, **deep** and **efficient**, and every role
points at one of them (synthesize, situations and judges → efficient; ask and
its retrieval loop → deep) instead of each role carrying its own endpoint.
Plumbing, done by whichever item below needs the second tier first. The claim
check and the gap namer already run on the synthesize endpoint under their own
response shapes (`HttpCompleter::for_claim_checking`, `for_gap_naming`), so the
split is a rename waiting for the first item that needs the deep tier elsewhere.

The ask harness and ask feedback — item 1 of the original list — are built
(spec `2026-08-17-ask-harness-design.md`): verdicts on the answer page with
carriers, `questions.json` in the export, `evaluate_ask` measuring citation
recall, abstention accuracy and faithfulness by literals and by claim check,
and "nothing here" surfaced as knowledge gaps on the capture page. The numbers
below now exist.

1. **Situation vectors.** People type situations; artifacts are embedded as
   passages, and a question matches a question far better than it matches a
   passage. At ingest the efficient model writes the three to five situations
   an artifact answers, each embedded as an additional named vector on the same
   Qdrant point, and both search and ask fuse passage hits with situation hits.
   Write-time inference, zero query cost, and the largest single retrieval gain
   on the list. Text untouched; the harness decides whether it stays on.

2. **Streaming ask.** The completion streams to the page — reasoning tokens
   included, when the deep model emits them — so the operator watches the model
   think instead of a spinner. `[n]` in the answer is a link: it opens the
   artifact in the detail pane, hover shows the excerpt, and the excerpt rail
   lights up where it is cited. And a **capture this answer** button, which
   stores the answer as an ordinary corpus with source `ask`, the question and
   the cited artifact ids as provenance, editable before it is saved. That is
   the operator pasting something, not the system writing memory: the trace
   records that the text was model-written and from what, and synthesis then
   treats it like any other paste.

3. **The retrieval loop.** Built on streaming, so each step is visible as it
   happens. Pack to the relevance cliff, not to the window — noise excerpts
   make the answer worse and the call dearer. Pull the neighbours of the top
   hits (same corpus, adjacent ordinal; one-hop associations) as candidates,
   because the answer is often in the artifact next to the one that matched.
   Let the model say once what it still needs and retrieve again before it
   answers, which is what turns single-hop into multi-hop, at the cost of one
   bounded extra call. And a **literal check on the answer**: the same guard
   synthesis already runs — numbers, commands, paths — applied to what the
   model wrote. A literal that appears in no cited excerpt marks its sentence
   unsupported on the page rather than being read as fact. No inference; this
   is the fidelity thesis extended to generation.

<!-- CUT: automatic answer cards, and answers stored as artifacts without the
     operator asking. A synthesised digest competing in search with the exact
     wording it was derived from is fidelity loss by design; the capture button
     is the operator's decision, recorded as such, and that is the line.
     CUT: LLM excerpt compression at query time (extract the relevant
     sentences before answering). One more call to shave tokens off the next
     one; the cliff and the reranker do the same for free. -->

## [Retrieval]

What ask learns at write time, search inherits. From the [Ask] list: situation
vectors (item 1) change what search matches against; ask verdicts join judged
searches as access cues under **access reconsolidation** above. The cliff is
built (`search::cliff`, spec `2026-08-17-cliff-design.md`) and waits for ask's
retrieval loop to pack to it. The items here are search's own.

- **Continues in.** A hit whose neighbour in its corpus (adjacent ordinal) is
  also above the cliff says so, and one click reads on. The answer to a
  situation is often the paragraph after the one that matched.
- **Server-side grouping.** The per-corpus cap is applied client-side over a
  candidate pool three times the limit; a corpus whose artifacts fill the pool
  leaves nothing to promote. Qdrant's `query/groups` retrieves per group.
  `cap_per_corpus` in `src/core/search.rs` becomes the in-memory fallback.
- **Reranking on by default**, once there is a default endpoint worth assuming.
  A cross-encoder, not a model call; the harness — both of them — decides.

<!-- CUT: late-interaction reranking (ColBERT-style multivectors). A vector per
     token per artifact wrecks storage and memory in Qdrant and adds a model
     dependency, to beat a baseline that atomic, LLM-synthesised artifacts and
     hybrid search already make strong. -->

## [Core Platform & Tooling]

- **PDF capture**, then a **CLI**. A PDF corpus implements the same `CorpusView`
  trait as text and images — extracted text, a page map, `page 42` as the
  label — and the detail pane needs no changes.
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
