# Roadmap

Not built yet, roughly in the order it would be worth building.

## Retrieval

- **Evaluation on a real corpus.** Ranking now has several knobs — fusion,
  per-source cap, recency weight, pinned boost — and no way to measure whether a
  change helped. A fixed set of query/expected-chunk pairs would make that
  falsifiable. Everything below this line is guesswork until it exists.
- **Reranking on by default**, once there is a default endpoint worth assuming.
- **Late-interaction reranking** (ColBERT-style multivectors) as a prefetch stage
  inside Qdrant, replacing the external reranker hop. Needs a model dependency.
- **Server-side grouping.** The per-source cap is applied client-side, so a
  source whose chunks fill the entire candidate pool still crowds others out.
  Qdrant's `query/groups` fixes that properly.

## Features

- **Related chunks.** Qdrant's `recommend` takes a point id and returns its
  neighbours — a "more like this" panel for free, no embedding call.
- **Corpus map.** The distance-matrix API gives pairwise distances over a
  filtered subset: near-duplicate detection on ingest, and a real rendering of
  the neighbour graph the logo depicts.
- **Facets.** Tag and category counts for the Browse sidebar, straight from the
  payload index instead of a SQL scan.
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
- `clippy` is not run locally in every environment; CI is the only gate.
