# Personal Knowledge Base — High-Level Concept

## 1. Purpose

A self-hosted store for reusable, discrete pieces of knowledge (forensic methods, admin procedures, everyday how-tos) that:

- is faster and more efficient to write into than typing a fresh AI prompt each time,
- is faster to retrieve from than waiting on LLM inference,
- retrieves by meaning, not exact keywords,

Retrieval is a **search problem**, not a **chat problem**. Generation is an optional layer on top, never the default path.

## 2. Core principles

1. **Ingest is cheap, processing is deferred.** Dropping content in should never block on AI calls. Enrichment happens async, after the fact.
2. **Search returns matches, not prose.** Default query path: embed → vector search → (optional) rerank → return ranked chunks. No completion call in the hot path.
3. **Inference is a pluggable dependency, not a component.** One OpenAI-compatible endpoint (your own) provides three interchangeable roles: chunking/structuring, embedding, reranking. Any of the three can be swapped independently.
4. **Multiple front doors, one backend.** Web UI, MCP server, and (later) CLI are all thin clients over a single internal API — no logic duplicated per interface.
5. **Never lose the source.** Raw ingested text is always stored verbatim, independent of how chunking/embedding turns out. Reprocessing must be possible without re-ingesting.

## 3. Architecture overview

```
                ┌─────────────┐   ┌─────────────┐   ┌────────────┐
                │   Web UI    │   │  MCP Server │   │ CLI (later)│
                └──────┬──────┘   └──────┬──────┘   └──────┬─────┘
                       │                 │                 │
                       └────────┬────────┴────────┬────────┘
                                │  Core REST API   │
                                └────────┬─────────┘
                    ┌───────────────────┼───────────────────┐
                    │                   │                   │
              ┌─────▼────-─┐      ┌──────▼──────┐      ┌──────▼──────┐
              │  database  │      │  Job Queue  │      │   Qdrant    │
              │ (raw docs, │      │ (async proc)│      │  (vectors + │
              │  chunk     │      │             │      │   payload)  │
              │  metadata) │      │             │      │             │
              └────────────┘      └──────┬──────┘      └─────────────┘
                                          │
                                   ┌──────▼──────┐
                                   │  Inference   │
                                   │  API (yours) │
                                   │  - chunk     │
                                   │  - embed     │
                                   │  - rerank    │
                                   └──────────────┘
```

## 4. Ingest pipeline

1. **Capture** — paste/drop a blob of text (or a file) via any front door. Stored immediately, unmodified, in Database. Returns instantly — no waiting.
2. **Segment (background, LLM-assisted)** — a chunking pass splits the raw blob into atomic, self-contained units (one technique / one procedure / one fact per chunk), rather than naive fixed-length windows. This is the step that makes retrieval precise later — bad chunking is the most common cause of bad semantic search.
3. **Enrich (background, optional)** — light metadata pass per chunk: title, category, tags. Cheap, improves filtering and browsing, not required for search to function.
4. **Embed (background)** — each chunk sent to your embedding endpoint, vector + payload (chunk text, source id, tags, timestamps) written to Qdrant.
5. **Failure handling** — each stage is idempotent and retryable per-chunk; a raw doc stuck at "segmented but not embedded" is still visible and re-processable, never silently dropped.

## 5. Retrieval pipeline

1. Query text → embed via same inference API.
2. Qdrant similarity search → top-N candidates.
3. Optional rerank pass (cheap/fast model call, not full generation) to reorder top-N by relevance.
4. Return chunk text + source + score directly. This *is* the answer — no completion call required.
5. Optional: a separate "ask" mode layers a generation step on top of the same retrieved chunks, for cases where synthesis across multiple chunks is actually wanted. Kept clearly separate from default search so it's never the accidental slow path.

## 6. Interfaces

- **Web UI**: capture box, instant search bar, source/chunk browser, basic edit/delete. Optimized for speed of capture and speed of lookup, not writing experience.
- **MCP server**: exposes `ingest` and `search` (and maybe `ask`) as tools — lets Claude Code, Claude Desktop, or other agents write into and query the base directly during a session. This is your bridge from "I just explained a procedure to an AI" to "it's now in permanent storage."
- **CLI (later)**: thin wrapper over the same REST API — same tools, terminal-native.

## 7. Data model (sketch)

- **Source**: id, raw_text, created_at, status (raw / segmented / embedded / failed)
- **Chunk**: id, source_id, text, tags[], category, created_at
- **Vector (Qdrant)**: chunk_id (payload link), embedding, tags[] (for filtered search), category

## 8. Explicit non-goals (v1)

- No cross-device sync beyond "it's a web app you can reach"
- No chat-first UX — generation stays opt-in, secondary

