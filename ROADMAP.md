# Roadmap

Not built yet, roughly in the order it would be worth building.

The ordering assumes the shape of use engram is for: paste a long reference
document once, and a year later find the one paragraph in it that answers the
situation you are in — while typing the situation, not after formulating a
query. The pipeline from capture to ranked chunk is built. Most of what follows
is the last hop, from a ranked chunk to the command the reader actually runs,
plus the means to tell whether the ranking was any good.

## Recall surface

The result list is where a year-old capture either pays off or does not. Today
it is a flat list of whole chunks.

- **Chunk detail view.** A result card links to `/ui/sources/{id}`, which
  renders the raw document and every chunk it produced. Clicking a hit should
  land on the hit: a `/ui/chunks/{id}` route showing the chunk, its title,
  category, tags, its span in the source, and a link back to the surrounding
  raw text. Without it, "open the details" means scrolling a chapter to find
  the paragraph search already found.
- **Snippet and expand.** `_results.html` renders the whole chunk body. A
  self-contained chunk is small by design, but the segmenter's size limit is a
  prompt hint, not a guarantee, and one oversize chunk pushes every other
  result off the screen. Clamp the card to a few lines with an expand control,
  keeping fenced code blocks intact when clamping.
- **Query term highlighting.** The sparse branch already knows which terms
  matched; the reader gets no mark. In a chunk that is half prose and half
  shell, highlighting the terms that hit is the difference between reading a
  card and scanning one.
- **Copy the command.** A copy button per fenced code block. The point of
  keeping literals verbatim is that they get run.
- **Tag and category controls.** `UiSearchParams` accepts `tags` and
  `category`, and the search page renders no input for either, so narrowing to
  `category=procedure` is API-only. Chips beside the search box, ideally
  populated from facet counts (see below).

## Search-as-you-type

`keyup changed delay:250ms` is the whole of the incremental behaviour.

- **Query embedding cache.** Every burst of typing costs one embedding call
  before Qdrant is touched at all. Prefixes repeat constantly within a single
  search, and identical queries repeat across sessions. A small bounded cache
  keyed on the trimmed query removes the dominant latency term for a local
  embedder and makes a remote one usable.
- **Do not mark partial queries as seen.** `Core::search` calls `mark_seen` on
  every request, so typing `d`, `dd`, `dd if` stamps `last_seen_at` on whatever
  each prefix happened to match. That is the same field `resurface` reads, so
  incremental search quietly drains the forgotten-chunk feature. Mark on an
  explicit action — submit, expand, open — or debounce the stamp well behind
  the query.
- **Latency budget in the response.** `search` already logs `embed_ms` and
  `total_ms`. Surfacing them in the UI, even faintly, makes it obvious whether
  a sluggish box is the embedder, the vector store or the reranker.

## Retrieval

- **Evaluation on a real corpus.** Ranking now has several knobs — fusion,
  per-source cap, recency weight, pinned boost — and no way to measure whether a
  change helped. A fixed set of query/expected-chunk pairs would make that
  falsifiable. Everything below this line is guesswork until it exists. The
  pairs worth writing first are the hard case: a query phrased as a situation
  in prose, against a chunk that is mostly commands and shares few of its
  words.
- **Reranking on by default**, once there is a default endpoint worth assuming.
- **Late-interaction reranking** (ColBERT-style multivectors) as a prefetch stage
  inside Qdrant, replacing the external reranker hop. Needs a model dependency.
- **Server-side grouping.** The per-source cap is applied client-side, so a
  source whose chunks fill the entire candidate pool still crowds others out.
  Qdrant's `query/groups` fixes that properly.

## Segmentation fidelity

The chunker is instructed to reproduce commands, paths and error strings
verbatim. Nothing checks that it did.

- **Literal verification.** After segmentation, extract fenced code, inline
  code and path-like tokens from each chunk and confirm each appears in the
  window it came from. A mismatch means the model paraphrased something that
  will later be pasted into a root shell. Flag the chunk, and prefer retrying
  the window over storing it silently.
- **Span verification.** `source_lines` comes back from the model and is
  trusted after the window offset is applied. Checking that the chunk's text
  plausibly overlaps the lines it claims would catch a whole class of confident
  nonsense, and the chunk detail view depends on the span being right.
- **Coverage report.** Windowing is tested to lose no line, but nothing tracks
  how much of a source ended up inside some chunk. A source where the segmenter
  quietly dropped half the chapter currently looks identical to one where it
  did not.

## Features

- **Near-duplicate detection on ingest.** Sources are deduplicated by an exact
  hash of the raw text, so re-pasting the same chapter a year later with one
  changed byte stores it twice and the two copies then compete for the same
  queries. The distance-matrix API below is the cheap way to catch this at
  capture time and offer to replace rather than add.
- **Related chunks.** Qdrant's `recommend` takes a point id and returns its
  neighbours — a "more like this" panel for free, no embedding call.
- **Corpus map.** The distance-matrix API gives pairwise distances over a
  filtered subset: near-duplicate detection on ingest, and a real rendering of
  the neighbour graph the logo depicts.
- **Facets.** Tag and category counts for the Browse sidebar, straight from the
  payload index instead of a SQL scan. Also what the search page's filter chips
  should be built from.
- **File upload** and a **CLI**.

## Operations

- **Quantization and on-disk payload** for small hosts. The chunk text is stored
  in both SQLite and the Qdrant payload; dropping the second copy and hydrating
  from SQLite would cut memory noticeably.
- **Snapshots** as part of the backup story, so restoring does not mean paying
  for every embedding again.
- **Multi-user**, via payload-partitioned tenancy (`is_tenant`) rather than a
  collection per user. Cheap to design in now, expensive to retrofit.
- **OAuth 2.1 for `/mcp`.**

## Loose ends

- The SQLite FTS5 index and its triggers exist and are tested, but nothing reads
  them. Hybrid search happens in Qdrant instead, where fusion is one round trip
  and the lexical index cannot drift from the vectors. Either wire FTS5 up as a
  fallback or delete it.
- Ingest is bounded by axum's default 2MB body limit rather than a deliberate
  one. Fine for pasted prose, wrong the moment file upload lands; set the limit
  explicitly and reject oversize captures with a message rather than a
  truncated form error.
- `clippy` is not run locally in every environment; CI is the only gate.
