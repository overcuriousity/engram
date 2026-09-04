# Roadmap

One item. Everything else is on the [issue
tracker](https://github.com/overcuriousity/engram/issues), which has been the
roadmap since `0298e4b` and stays it — this file is back for the one thing that
is a direction rather than a task, and it holds nothing else.

Under the item, the two lines the old file used. *Worth* is the difference, not
the feature: what an operator can do afterwards that they could not before.
*Cost* names what has to be touched and then one of three sizes. **One commit**
is a day or less, one or two files, nothing to measure. **A branch** is several
files across layers, tests of its own, possibly a migration. **A project** wants
a design record in `docs/superpowers/specs/` before any code.

## A first run that costs one process

Today engram needs three things before it stores a sentence: Qdrant, an
embedding endpoint, and — since the 2026-09 capture reshape — a chat model.
That is a defensible production posture and an indefensible first impression.
Nobody evaluates a place to keep their notes by standing up two services and a
GPU first, so the people who would benefit most from verbatim recall with
provenance never reach the part that would show them.

The destination: `curl | sh`, `engram`, open the browser, paste something,
search it. One process, no config file, no GPU. `[infer.tiers.*]` then upgrades
that into what engram already is. The three services do not go away — they
become the second step instead of the first.

Three steps, in order. Each is worth shipping alone.

### 1. An embed-only floor

`src/jobs/passages.rs` is already headed *"Capture without synthesis: a window
becomes verbatim passages sized to the embedder"*, and it calls no completer.
It is what every large capture does now. Two things make a chat model
mandatory anyway: the `ok_or_else` at `src/config.rs:1194`, and the size fork
at `passages.rs:243` that arms synthesis when a capture fits one call. Make the
role optional and let every capture take the verbatim path when it is absent.

Ask, the judged capture, reminders, the journal, promotion and the day page go
dark without it. They must say so on the surface where they would have
appeared, not fail — a door that is not configured is not a door that is
broken, and `PkdbTools::routes` already draws that distinction for `ask` by
removing the tool rather than answering "not configured".

- **Worth.** Capture, hybrid search and verbatim passages with no chat model at
  all. On its own this does not finish the installer, but it is the only step
  of the three that removes a *requirement* rather than a *service*, and
  nothing else here is reachable while capture depends on a generation.
- **Cost.** `src/config.rs`, `src/jobs/passages.rs`, and the surfaces that
  announce an absent role. One commit, plus the honesty work on the surfaces.

### 2. The embedder in the binary

`candle` runs BERT-family embedders on CPU in pure Rust. That is the reason for
it rather than `ort` or `fastembed`: the x86_64 release is `musl`-static, and
an ONNX runtime fights that.

The weights do not go in the binary. `infer::budget::fetch_blocking` already
downloads the tokenizer on first boot, parses before it caches, re-fetches a
cached file that no longer parses, and falls back to a bundled default — a
model wants exactly that, cached under `store.dir`. The shipped binary is 30 MB
stripped, with an 11 MB `tokenizer.json` already inside it, so the shipping
story does not change shape.

A bundled small embedder is symmetric and 384-dimensional where the configured
default is EmbeddingGemma's asymmetric 768. That is a different recipe, so it is
a different collection, and the fingerprint check at boot already says so. It
is the right default for a first run and the wrong one to migrate a real base
onto — which is the whole of why a base that has grown past this should move to
a served endpoint and re-capture while it is still cheap.

- **Worth.** The second service goes away. Combined with step 1, a first run
  needs Qdrant and nothing else.
- **Cost.** A new module under `src/infer/`, the role resolution that picks it,
  and the first-boot fetch. A branch.

### 3. A local vector store that keeps both halves of retrieval

`src/vector/memory.rs` implements nearly the whole `VectorStore` trait —
`context_query`, `facets`, `neighbours`, `resurface`, `stale_candidates`,
lifecycle. It is not the drop-in it looks like: `search` at `memory.rs:310`
takes `_sparse` and ignores it. Shipping it as the local backend as it stands
would delete BM25, which is the half of retrieval that finds `E01` and
`--dry-run`, and losing that to gain an easy install trades away the reason to
prefer engram in the first place.

So the work is a sparse posting list and the document-frequency statistics
Qdrant supplies today through its `"modifier": "idf"` — SQLite holds both
without complaint — and then persistence, vectors as blobs in the per-user
database rather than a `RwLock<HashMap>` that dies with the process. Brute
force is the right search: 100k artifacts at 768 dimensions is about 300 MB
resident and tens of milliseconds a query with the filter applied first. One
person does not need an approximate index, and adding one before they do is the
kind of tier `lean beats clever` refuses.

- **Worth.** The last service goes away, and the installer is the install.
- **Cost.** `src/vector/`, the config that selects a backend, and the sparse
  half written twice over. A project — the 57 `#[ignore]`d Qdrant tests are the
  contract it has to meet, and they are the design record it can be written
  against.

### The risk this item carries

Two backends is two behaviours, and the ones that differ are the subtle ones:
the recency term folded into the score, the lifecycle filters, the generation
and alias dance behind `--reindex`. A local backend that ranks *nearly* the
same is worse than one that ranks visibly differently, because it moves the one
figure comparable across months without anyone noticing.

The rule for this item, then, and it is not negotiable by convenience: the
evaluation harness runs on both backends, over the same exported corpus, and
the two reports are read side by side before the local one is offered as a
default. A harness that exists to hold ranking still cannot be exempted by the
change most likely to move it.
