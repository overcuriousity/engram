# Roadmap

Not built yet, roughly in the order it would be worth building.

The ordering assumes the shape of use engram is for: paste a long reference
document once, and a year later find the one paragraph in it that answers the
situation you are in — while typing the situation, not after formulating a
query. The pipeline from capture to ranked chunk is built, as is the last hop from a
ranked chunk to the command the reader runs: the search page pairs a ranked rail
with the chunk beside its source lines, and segmentation now verifies that
literals and spans survived the rewrite. What remains is mostly the means to
tell whether the ranking is any good.

## Recall surface

The workspace pairs a ranked rail with a detail pane. What it still lacks is a
way to narrow the list without editing a URL.

- **Tag and category controls.** `UiSearchParams` accepts `tags` and
  `category`, and the search page renders no input for either, so narrowing to
  `category=procedure` is API-only. Chips beside the search box, ideally
  populated from facet counts (see below).

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
- **Server-side grouping.** The per-source cap is applied client-side, over a
  candidate pool three times the limit. It reorders rather than truncates — what
  it displaces refills the tail — but a source whose chunks fill the entire
  candidate pool still leaves nothing to promote ahead of it. Qdrant's
  `query/groups` fixes that at the source, by retrieving per group.

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
- **File upload**, **PDF** and a **CLI**. The detail pane asks a `SourceView`
  for the lines a chunk claims, so a PDF source implements the same trait —
  extracted text, a page map, `page 42` as the label — and the pane needs no
  changes. Upload comes first; the body limit is explicit now, at 8 MB.

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
  fallback or delete it. Its triggers are now scoped to `text`, `title` and
  `tags`, so at least it no longer pays for every job-status write.
- **Term-id collisions.** Sparse dimensions are `u32` and terms are hashed into
  them, so a large enough vocabulary conflates two terms into one dimension and
  a document matches a word it does not contain. `sparse::term_id` is the only
  place ids are derived, so replacing it with a vocabulary table is a contained
  change plus a `--reindex`. Nothing measures the effect yet, which is the
  evaluation gap above.
- Ingest is bounded by axum's default 2MB body limit rather than a deliberate
  one. Fine for pasted prose, wrong the moment file upload lands; set the limit
  explicitly and reject oversize captures with a message rather than a
  truncated form error.
- `clippy` is not run locally in every environment; CI is the only gate.
