# Roadmap

Not built yet, roughly in the order it would be worth building.

The ordering assumes the shape of use engram is for: paste a long reference
document once, and a year later find the one paragraph in it that answers the
situation you are in — while typing the situation, not after formulating a
query. The pipeline from capture to ranked artifact is built, as is the last hop
from a ranked artifact back to where it came from: the search page pairs a ranked
rail with the artifact beside its corpus lines, and synthesis now verifies that
the details it must not alter — names, dates, figures, quoted wording — survived
the rewrite.

It also assumes the thesis: inference happens at write time, not read time. A
question costs one embedding and one vector search, never a generation. So the
way to make retrieval better is to make the background job do more, not to add
a model call to the query path, which is what the write-time section below is
for.

The evaluation harness is built and is not on this list: `cargo test --test
eval` scores hand-written query/artifact pairs against a corpus that stays on
the operator's own machine. It is unpopulated by design — writing pairs and
freezing a corpus costs real GPU time, and it is worth spending only when a
decision actually turns on the answer. The items under *Write-time inference*
are exactly such decisions: both add vectors to the index, and adding vectors
always looks like it is helping.

## Recall surface

The workspace pairs a ranked rail with a detail pane. What it still lacks is a
way to narrow the list without editing a URL.

- **Tag and category controls.** `UiSearchParams` accepts `tags` and
  `category`, and the search page renders no input for either, so narrowing to
  `category=procedure` is API-only. Chips beside the search box, ideally
  populated from facet counts (see below).

## Retrieval

- **Reranking on by default**, once there is a default endpoint worth assuming.
- **Late-interaction reranking** (ColBERT-style multivectors) as a prefetch stage
  inside Qdrant, replacing the external reranker hop. Needs a model dependency.
- **Server-side grouping.** The per-corpus cap is applied client-side, over a
  candidate pool three times the limit. It reorders rather than truncates — what
  it displaces refills the tail — but a corpus whose artifacts fill the entire
  candidate pool still leaves nothing to promote ahead of it. Qdrant's
  `query/groups` fixes that at the source, by retrieving per group.

## Write-time inference

Synthesis already rewrites a passage into standalone artifacts. That is
speculation about *representation* — how the text should read when found alone.
The items here speculate about *access*: what the reader will be holding when
they come looking. Both are paid for once, in the background, and neither adds
anything to the query path.

- **A speculative query index.** The gap the evaluation pairs are meant to
  expose is a vocabulary gap. An artifact is written in the language of whoever
  wrote the corpus — a clause in a lease, a step in a recipe, a passage of
  case law, a line of a shell session — and the reader arrives with the words
  they happen to have at the time, which are usually the words of the
  situation, not of the document. Close the gap at write time. The segment job
  already has the model warm on the artifact it just produced, so have the same
  call also emit three to five questions the artifact answers, plus the other
  names for whatever the artifact is about — the everyday word for a term of art,
  the term of art for an everyday word. Embed those alongside the artifact text as
  extra points that
  resolve to the same `artifact_id`, dedupe by `artifact_id` after retrieval, and
  score an artifact by its best-matching point. Costs vectors and one longer
  synthesis reply; costs the query path nothing. Needs the per-corpus cap
  and `cap_per_corpus` to work on artifact identity rather than point identity,
  and wants pairs written before it is built — this is the change most likely
  to look good and rank worse.
- **Precomputed answer cards.** A question asked twice should not be
  synthesised twice. Cluster neighbouring artifacts in the background, run one
  `ask`-shaped completion per cluster offline, and store the result as an
  ordinary artifact with `category=digest` and a link to its members. Retrieval
  needs no new path — the card competes with its own corpora and usually wins,
  because it was written to answer rather than to document. Only worth building
  after the eval harness can show a card beating the artifacts it was made from;
  otherwise it is a paraphrase that quietly outranks the exact wording it was
  made from, which is the one failure mode this design exists to avoid. Cards must be
  regenerated, not amended, when a member artifact changes.

## Features

- **Near-duplicate detection on ingest.** Corpora are deduplicated by an exact
  hash of the raw text, so re-pasting the same chapter a year later with one
  changed byte stores it twice and the two copies then compete for the same
  queries. The distance-matrix API below is the cheap way to catch this at
  capture time and offer to replace rather than add.
- **Consolidation and staleness.** Near-duplicate detection above catches the
  collision at capture; nothing catches the pair that drifts apart afterwards.
  A background sweep over the distance matrix can flag two kinds of pair: near
  identical, where the older one should be superseded rather than left to
  compete for the same queries, and same subject with a detail that disagrees,
  where two artifacts give a different number, date, name or step for the same
  thing. The second is the one that matters — much of what is worth keeping
  goes quietly out of date within a year of keeping it,
  and staleness is invisible today because a wrong artifact ranks exactly as well
  as a right one. Flag rather than delete: `superseded_by` on the artifact row, a
  filter that hides superseded artifacts from search by default, and a review
  queue on Ops. Deciding which of two contradictory artifacts is current is a
  judgement the reader has to make.
- **Related artifacts.** Qdrant's `recommend` takes a point id and returns its
  neighbours — a "more like this" panel for free, no embedding call and no
  completion, so it is the cheapest item on this page and the one that best
  matches what the detail pane is already for. Also the substrate the two items
  above need: clustering for cards and pair-finding for consolidation are the
  same neighbour query, batched.
- **Corpus map.** The distance-matrix API gives pairwise distances over a
  filtered subset: near-duplicate detection on ingest, and a real rendering of
  the neighbour graph the logo depicts.
- **Facets.** Tag and category counts for the Browse sidebar, straight from the
  payload index instead of a SQL scan. Also what the search page's filter chips
  should be built from.
- **File upload**, **PDF** and a **CLI**. The detail pane asks a `SourceView`
  for the lines an artifact claims, so a PDF corpus implements the same trait —
  extracted text, a page map, `page 42` as the label — and the pane needs no
  changes. Upload comes first; the body limit is explicit now, at 8 MB.

## Operations

- **Quantization and on-disk payload** for small hosts. The artifact text is stored
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
  change plus a `--reindex`. Whether it is worth the reindex is a question the
  eval harness answers and nothing else does.
- Ingest is bounded by axum's default 2MB body limit rather than a deliberate
  one. Fine for pasted prose, wrong the moment file upload lands; set the limit
  explicitly and reject oversize captures with a message rather than a
  truncated form error.
- `clippy` is not run locally in every environment; CI is the only gate.
