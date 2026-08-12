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
eval` scores query/artifact pairs against a corpus that stays on the operator's
own machine. The pairs are no longer hand-written, which was the thing standing
in the way — a query composed while reading an artifact borrows its vocabulary,
and every retrieval system passes such a pair. With `feedback.enabled`, real
searches are recorded as they are made and judged afterwards at `/ui/judge`;
`--export-eval` hands the result to the harness. The corpus comes out of the
live database, so it costs no GPU time to freeze and keeps its production ids.

What remains true is that the harness is the only figure comparable across
months: it runs against a frozen corpus, while the field value on the judging
page describes how search behaved on the day it was used.

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
     RRF-fused in one round trip) precise enough. -->

## [Write-Time Inference & Hygiene]

Synthesis already rewrites a passage into standalone artifacts. That is
speculation about *representation* — how the text should read when found alone.
Speculation about *access* is what the reader will be holding when they come
looking. Both are paid for once, in the background, and neither adds anything
to the query path.

- ~~**Near-duplicate detection on capture.**~~ Done — a bottom-k MinHash over
  word shingles (`src/store/shingle.rs`) is computed at capture and compared
  against every stored corpus. Above `consolidate.near_dupe_min` the capture is
  stored verbatim and parked in `needs_review` rather than segmented, and Ops
  offers replace / keep both / discard. Shingles rather than the distance-matrix
  API as originally sketched: the collision is between *corpora*, which have no
  vectors until synthesis has already been paid for, and a hash of the raw text
  answers it before any of that is spent.
- ~~**Consolidation and staleness sweep.**~~ Done — `Stage::Consolidate`
  (`src/jobs/consolidate.rs`) runs on a timer, asks Qdrant's distance-matrix
  API for near pairs in one round trip (`VectorStore::near_pairs`), and splits
  them at two thresholds. At or above `auto_supersede` (0.95) the cluster
  collapses onto its newest member: `superseded_by` on the losing rows, a
  `superseded` payload flag that `SearchFilter` excludes by default, and an undo
  on Ops. Clustered rather than resolved pairwise, or A loses to B and B then
  loses to C, leaving A pointing at something hidden. Between `review_min`
  (0.88) and that, the pair goes on the `artifact_pairs` queue instead — 0.88 is
  where two genuinely distinct artifacts about one subsystem routinely sit.
  The staleness half is the opt-in `consolidate.judge`: queued pairs are
  filtered on fact-shaped tokens (`src/infer/facts.rs`) with no inference at
  all, and only a pair whose values actually differ reaches the model, which is
  asked one yes/no question under a per-sweep budget. Nothing is ever merged or
  deleted, and which of two contradictory artifacts is current stays the
  reader's judgement.
- **Caveats on artifacts.** Done as part of the above, and worth naming
  separately: the synthesis call now also returns the conditions under which an
  artifact does not apply. It costs output tokens on a call already being made,
  never another call. Stored and rendered, deliberately *not* embedded — see the
  speculative query index below for why anything that changes what every vector
  is built from waits for eval pairs.
- **Corpus map.** The distance-matrix API gives pairwise distances over a
  filtered subset — the same call the two items above need, plus a real
  rendering of the neighbour graph the logo depicts.

<!-- CUT: precomputed answer cards. A synthesised digest stored as an ordinary
     artifact competes with — and was written to beat — the exact source wording
     it was derived from. That is fidelity loss by design, and it buys back only
     latency the write-time thesis already spends. It also costs a second
     write-time LLM pass and a regeneration cascade on every member edit. -->

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
- ~~**Delete the FTS5 index and its triggers.**~~ Done — `src/store/schema.sql`
  never creates `artifacts_fts` or its three write triggers, and
  `keyword_search` / `fts_quote` are gone from `src/store/artifacts.rs`. Hybrid search runs in
  Qdrant instead — dense and sparse as prefetch branches fused by RRF in one
  round trip (`src/vector/qdrant.rs`) — where the lexical index cannot drift
  from the vectors. The SQLite index was never read by any production path and
  charged three triggers on every artifact write to stay current. It was
  external-content, so no text was lost with it.
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
