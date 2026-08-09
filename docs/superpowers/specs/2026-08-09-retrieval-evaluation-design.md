# Retrieval evaluation, and the removal of the structural fallback

## Why

Ranking has knobs — fusion, per-source cap, recency weight, pinned boost,
reranking on or off — and no way to tell whether turning one helped. The only
feedback available today is typing a few searches, which is the one test that
cannot work: you phrase the query using words you remember from the passage, so
it comes back first and everything looks fine. The case that actually fails —
half-remembering something and describing it in different words — is the case
hand-testing cannot reach.

This is a decision aid for forks that are already in front of us. Which
embedding model is better for German forensic material. Whether the reranker
earns its hop. Whether replacing the hashed sparse term ids with a vocabulary
table is worth a full reindex. Each of those is a real either-or with real
cost, and today each would be settled by feel.

It is also what the roadmap makes a precondition for everything under
*Write-time inference*. Both items there add generated vectors to the index,
and a speculative index that adds vectors always looks like it is doing
something. Whether the right chunk ranks higher is a different question.

What this is not: a claim about retrieval quality in general. The numbers
describe one corpus. Absolute values do not transfer between operators.
Direction mostly does — a change that drops recall on a reasonable corpus is
unlikely to be helping on another one.

## Scope

Two pieces of work, in one batch because the second contaminates the first.

1. An evaluation harness measuring recall@10 and MRR over hand-written
   query/chunk pairs.
2. Removal of the no-LLM structural fallback in segmentation. The LLM is a hard
   dependency from now on.

The fallback belongs here because the frozen chunks the harness ranks against
must all have come from the model. A window that silently fell back to a
blank-line split would put paragraph-shaped debris into the corpus and quietly
change what the benchmark measures.

## Corpus

The corpus is real study material — lecture scripts and legal texts — which is
**not permitted to be published**. It therefore lives entirely outside the
repository, at a directory named by `ENGRAM_EVAL_DIR`, defaulting to
`./eval-data`. That default path is added to `.gitignore` as a guard.

The repository receives the harness, the file format and the documentation.
It never receives corpus text, chunk text, or anything derived from them.

Contents of the eval directory:

```
corpus/*.txt      extracted source text, one file per document
chunks.json       frozen segmenter output
pairs.json        hand-written query/expected-chunk pairs
```

Roughly 1000 lines of text in total, drawn with `pdftotext` from three
documents in three different fields, so the benchmark is not a test of one
vocabulary:

- filesystem forensics (`Lehrbrief Dateisysteme`) — technical
- mobile forensics (`Grundlagen der Mobilfunkforensik`) — technical, other domain
- criminal law material under `Cybercrime I` — legal prose

The corpus is fixed once extracted. A document that changes invalidates every
score recorded before the change, because a score is only meaningful against a
score from the same corpus.

## Freezing the chunks

Search does not run over documents. It runs over chunks, and producing a chunk
is a model call: `split_into_windows` cuts the document locally, then
`Chunker::segment` rewrites each window into standalone passages.

Running that on every benchmark run would cost a completion per window, take
minutes, and return slightly different chunks each time — a two percent ranking
change would be indistinguishable from segmenter noise. So it runs once.

A binary, `eval-prepare`, ingests `corpus/*.txt` through the real segmenter and
writes `chunks.json`: chunk id, source file, text, title, category, tags. From
then on the harness reads that file, and needs no segmentation model at all.

Regeneration is deliberate: after a segmentation prompt change, rerun
`eval-prepare`, then re-check the pairs, since chunk ids will have changed.

## Pairs

The content of the benchmark, and the part that cannot be automated.

```json
{
  "query": "handy war aus als die polizei kam, komme ich trotzdem an die daten",
  "expect": "01J8ZK...",
  "note": "BFU / AFU state"
}
```

Roughly twenty pairs, drafted against the extracted text and then corrected by
the user, who is the only one who can say which passage was the wanted answer.

The pairs worth writing are the hard case the roadmap names: a query phrased as
a situation, in the words the reader happens to have, against a chunk written
in the vocabulary of its own field, sharing few or none of those words. A pair
whose query repeats the chunk's own terminology measures nothing — any retrieval
system passes it.

Queries are German, matching the corpus. Whether the embedding model handles
German well is itself one of the questions the harness exists to answer.

## Harness

`tests/eval.rs`, `#[ignore]`d, run explicitly:

```
cargo test --test eval -- --ignored --nocapture
```

Ignored for the same reason `tests/integration_qdrant.rs` is: it needs a real
Qdrant and a real embedding endpoint. The fake embedder in `core::test_support`
produces meaningless vectors, so a benchmark built on it would measure nothing.

Flow:

1. Read `ENGRAM_EVAL_DIR`; skip with a clear message if it is absent or empty,
   so a developer without the corpus gets an explanation rather than a failure.
2. Build a real `Core`: embedder from configuration, `QdrantVectors` against a
   throwaway collection, a temporary SQLite store, no reranker unless enabled.
3. Insert the frozen chunks and embed them.
4. Run each pair's query through `Core::search` — the same entry point the web
   page and MCP use, so fusion, the per-source cap, recency weighting and
   reranking are all exercised exactly as they are in production.
5. Find the rank of the expected chunk in the results.
6. Print the report; drop the collection.

Report:

```
20 queries over 143 chunks   (embed bge-m3, rerank off, recency 0.05, cap 3)
recall@10   0.75   (15/20)
MRR         0.52

missed:
  "handy war aus als die polizei kam…"        not in top 10
  "wie finde ich raus wann die datei…"        rank 8
```

Both metrics, because they answer different questions. Recall@10 asks whether
the answer was on the page at all. MRR asks how far down it was. A change can
easily improve one and hurt the other, and the choice between them is a
judgement about what a search page is for.

The list of misses is the part that is actually read. An aggregate number says
something moved; the misses say what.

## Settings under test

The harness reads ranking settings from the environment rather than from
compiled constants, so one run of a sweep script is a loop rather than a
rebuild. In scope:

- `recency_weight`, `recency_half_life_days`, `pinned_boost` — already
  configuration, passed to `VectorConfig`.
- per-source cap — currently the constant `MAX_PER_SOURCE` in
  `core/search.rs`; the harness needs to vary it, so `search_capped` is the
  entry point it calls.
- reranker present or absent — already configuration.
- embedding model — already configuration; comparing two models means running
  the harness twice against two collections.

No production default changes as part of building the harness. Defaults change
afterwards, as a separate commit, with the measured numbers in the message.

## Removing the structural fallback

`fallback_pending_windows` in `jobs/segment.rs` currently rescues a window whose
retries are spent by splitting it on blank lines, storing the paragraphs
verbatim, and marking the window `Fallback`. That produces chunks that are not
what the rest of the system assumes a chunk is: no title, no category, no tags,
not rewritten to stand alone.

The model is a hard dependency. A window it will not segment stays unsegmented.

Changes:

- Delete `fallback_pending_windows` and `structural_chunks`, and the branch in
  `jobs/mod.rs` that calls the former.
- `WindowState::Fallback` becomes `WindowState::Failed`, carrying the model's
  own error text as its reason.
- A source with at least one failed window and at least one successful one ends
  `Partial`, as it does today. A source where every window failed ends `Failed`.
- Coverage already records how much of the document ended up inside a chunk, so
  the hole a failed window leaves is measured rather than merely logged.

Failures worth expecting, with the network healthy: unparsable model output
(the common one, and usually deterministic — the same window fails the same way
every attempt); a window that measures under budget with the estimating token
counter but exceeds the server's real context; a refusal on content the model
declines to process; and transient server-side resource failures, which the
existing retries already clear. `OpenAiChunker::segment` already makes one
repair attempt with the parser error fed back, and that stays — it is the
mitigation that actually works.

## Testing

Fallback removal is ordinary unit-testable work, and the existing tests in
`jobs/segment.rs` already cover the paths that change: a failing chunker must
now leave the window `Failed` rather than fall back, a partially failing source
must still embed the windows that succeeded, and a source where everything
failed must end `Failed` with no chunks.

The harness itself is a test, so it is verified by running it. Its own
mechanics — parsing `pairs.json`, computing recall and MRR from a rank list —
are pure functions and get unit tests with synthetic ranks, so a broken metric
is caught without a corpus.

## Out of scope

Tuning the defaults. That is the run that happens after this lands, and its
result is a separate commit.

Any change to ranking behaviour. A benchmark and a change to the thing it
measures must not arrive together, or the first numbers have nothing to be
compared against.
