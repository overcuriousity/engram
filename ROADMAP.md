# Roadmap

Not built yet, roughly in the order it would be worth building.

The ordering assumes the shape of use engram is for: paste a long reference
document once, and a year later find the one paragraph in it that answers the
situation you are in — while typing the situation, not after formulating a
query. The pipeline from capture to ranked artifact is built, as is the last hop
from a ranked artifact back to where it came from: the search page pairs a
ranked rail with the artifact beside its corpus lines, and synthesis verifies
that the details it must not alter — names, dates, figures, quoted wording —
survived the rewrite. Narrowing no longer means editing a URL: the search page
renders filter chips from Qdrant's facet counts, and the detail pane lists an
artifact's nearest neighbours.

Three constraints decide what is on this list and what was cut from it.

**Inference happens at write time, not read time.** A question costs one
embedding and one vector search, never a generation. Making retrieval better
means making the background job do more, never adding a model call to the query
path.

**Fidelity outranks convenience.** Retrieval returns the exact source artifact.
A paraphrase or a synthetic summary must never silently replace or outrank the
original wording — that is the one failure mode this design exists to avoid.

**Lean beats clever.** Anything that adds a storage tier, a model dependency or
a layer crossing without a measured retrieval gain does not go in.

The evaluation harness is built and is not on this list: `cargo test --test
eval` scores hand-written query/artifact pairs against a corpus that stays on
the operator's own machine. It is unpopulated by design — writing pairs and
freezing a corpus costs real GPU time, and it is worth spending only when a
decision actually turns on the answer. The speculative query index and the
term-id change below are exactly such decisions: both change what is in the
index, and adding to an index always looks like it is helping.

## [Retrieval]

- **Server-side grouping.** *(highest value)* The per-corpus cap is applied
  client-side, over a candidate pool three times the limit. It reorders rather
  than truncates — what it displaces refills the tail — but a corpus whose
  artifacts fill the entire candidate pool still leaves nothing to promote
  ahead of it. Qdrant's `query/groups` fixes that at the source, by retrieving
  per group. `cap_per_corpus` in `src/core/search.rs` becomes a fallback for
  the in-memory store only.
- **Reranking on by default**, once there is a default endpoint worth assuming.
  External reranker hop, unchanged; the only open question is what to assume
  when no endpoint is configured.

<!-- CUT: late-interaction reranking (ColBERT-style multivectors). A vector per
     token per artifact wrecks storage and memory in Qdrant and adds a model
     dependency, to win against a baseline that is already strong: atomic,
     LLM-synthesised artifacts make single-vector hybrid search (dense + sparse,
     RRF-fused in one round trip) precise enough. Speculative queries buy more
     recall per byte. -->

## [Write-Time Inference & Hygiene]

Synthesis already rewrites a passage into standalone artifacts. That is
speculation about *representation* — how the text should read when found alone.
Speculation about *access* is what the reader will be holding when they come
looking. Both are paid for once, in the background, and neither adds anything
to the query path.

- **A speculative query index.** The gap the evaluation pairs are meant to
  expose is a vocabulary gap. An artifact is written in the language of whoever
  wrote the corpus — a clause in a lease, a step in a recipe, a passage of case
  law, a line of a shell session — and the reader arrives with the words they
  happen to have, which are the words of the situation, not of the document.
  Close the gap at write time. The segment job already has the model warm on
  the artifact it just produced, so have the same call also emit three to five
  questions the artifact answers, plus the other names for whatever it is about
  — the everyday word for a term of art, the term of art for an everyday word.
  Embed those as extra points resolving to the same `artifact_id`, dedupe by
  `artifact_id` after retrieval, and score an artifact by its best-matching
  point. Speculative points are an access path only: they are never rendered
  and never returned as text, so the artifact the reader sees stays the exact
  one that was stored. Costs vectors and one longer synthesis reply; costs the
  query path nothing. Needs the per-corpus cap and grouping to work on artifact
  identity rather than point identity, and wants eval pairs written first —
  this is the change most likely to look good and rank worse.
- **Near-duplicate detection on capture.** Corpora are deduplicated by an exact
  hash of the raw text, so re-pasting the same chapter a year later with one
  changed byte stores it twice, and the two copies then compete for the same
  queries. The distance-matrix API is the cheap way to catch this at capture
  time and offer to replace rather than add.
- **Consolidation and staleness sweep.** Near-duplicate detection catches the
  collision at capture; nothing catches the pair that drifts apart afterwards.
  A background sweep over the distance matrix flags two kinds of pair: near
  identical, where the older one should be superseded rather than left to
  compete, and same subject with a detail that disagrees — two artifacts giving
  a different number, date, name or step for the same thing. The second is the
  one that matters: much of what is worth keeping goes quietly out of date
  within a year, and staleness is invisible today because a wrong artifact
  ranks exactly as well as a right one. Flag, never delete: `superseded_by` on
  the artifact row, a filter that hides superseded artifacts from search by
  default, and a review queue on Ops. Deciding which of two contradictory
  artifacts is current is a judgement the reader has to make.
  `VectorStore::neighbours` is the query this is built from — one artifact at a
  time today, batched over the collection for a sweep.
- **Corpus map.** The distance-matrix API gives pairwise distances over a
  filtered subset — the same call the two items above need, plus a real
  rendering of the neighbour graph the logo depicts.

<!-- CUT: precomputed answer cards. A synthesised digest stored as an ordinary
     artifact competes with — and was written to beat — the exact source wording
     it was derived from. That is fidelity loss by design, and it buys back only
     latency the write-time thesis already spends. It also costs a second
     write-time LLM pass and a regeneration cascade on every member edit.
     Speculative queries reach the same artifacts without inventing text. -->

## [Core Platform & Tooling]

- **File upload**, then **PDF**, then a **CLI**. The detail pane asks a
  `SourceView` for the lines an artifact claims, so a PDF corpus implements the
  same trait — extracted text, a page map, `page 42` as the label — and the
  pane needs no changes. Upload comes first; the body limit is explicit now, at
  8 MB.
- **Backup and restore.** Qdrant snapshots plus the SQLite file, restored
  together, so recovery does not mean paying for every embedding again. The
  snapshot is the artifact of record for a rebuild; a reindex is the fallback,
  not the plan.
- ~~**Delete the FTS5 index and its triggers.**~~ Done — `migrations/0008_drop_fts.sql`
  drops `artifacts_fts` and its three write triggers, and `keyword_search` /
  `fts_quote` are gone from `src/store/artifacts.rs`. Hybrid search runs in
  Qdrant instead — dense and sparse as prefetch branches fused by RRF in one
  round trip (`src/vector/qdrant.rs`) — where the lexical index cannot drift
  from the vectors. The SQLite index was never read by any production path and
  charged three triggers on every artifact write to stay current. It was
  external-content, so no text was lost with it.
- **Term-id collisions.** Sparse dimensions are `u32` and terms are hashed into
  them, so a large enough vocabulary conflates two terms into one dimension and
  a document matches a word it does not contain. `sparse::term_id` is the only
  place ids are derived, so replacing it with a vocabulary table is a contained
  change plus a `--reindex`. Build it only if the eval harness shows the
  collisions costing real hits — nothing else answers that.
- **OAuth 2.1 for `/mcp`.** OIDC login for the web UI is built
  (`src/auth/oidc.rs`: discovery, PKCE, nonce, subject/email allowlist); the
  MCP surface still authenticates on its own terms.
- **Multi-user tenancy.** *(de-prioritised)* Payload-partitioned
  (`is_tenant`) rather than a collection per user. Single-operator use is the
  design point and OIDC already gates who gets in, so this stays a note about
  which way to jump, not scheduled work.
- `clippy` is not run locally in every environment; CI is the only gate.

<!-- CUT: quantization and on-disk payload for small hosts. Dropping the Qdrant
     text payload to hydrate artifacts from SQLite makes every search cross a
     layer boundary it does not cross today, coupling read latency to a second
     store to save memory nobody has run out of. Text stays in the Qdrant
     payload. -->
