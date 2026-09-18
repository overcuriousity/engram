# Contained mode: an engram on the phone

Written 2026-09-18, against the tree as it stands on `claude/review-106-oxulgp`,
with the Android app at client parity with the web interface. This is the
second implementation the comment on `Reader` promises: an engram that lives
on the device and needs no server.

## What it is for

Today the app is a client. Without a paired server it captures into a queue
and shows nothing. Contained mode makes the phone a whole engram: it ships
with a vector store, an embedder, a reranker, a speech model and, where the
phone can carry one, a model that answers questions. It is the default for a
new install. Connecting to a server remains possible, for the speed of a real
GPU and for the doors only a server has, and that is exactly today's
behaviour.

The baseline device is a Pixel 8 running GrapheneOS: a Tensor G3, 8 GB of
memory, no Google Play Services, hardened malloc, memory tagging. Everything
below is sized to it.

## Decisions taken

- **Retrieval first.** Capture, extraction, chunking, embedding, search,
  reranking and ask are the contained product. The LLM-driven background work
  is opportunistic, and no screen depends on it having run.
- **The Rust core runs in the app, behind its own HTTP API on loopback.** No
  Kotlin reimplementation, no second API surface.
- **sqlite-vec replaces Qdrant.** The whole base is one SQLite file.
- **llama.cpp and whisper.cpp on the CPU.** The TPU is out of the design; see
  "Left out".
- **Models are downloaded, never shipped in the APK.** The retrieval set is
  required; the ask model and the speech model are each offered where they are
  first wanted.
- **The two modes share nothing.** Switching never moves, merges or deletes a
  base.

## 1. Modes and the seam

Two modes, **contained** and **server**, held as one stored value and read in
one place: where `Engram.kt` builds its `Reader`.

- Server mode: `ServerReader` over the paired connection, as now.
- Contained mode: the same `ServerReader`, over a `Connection` to
  `http://127.0.0.1:<port>` carrying a token minted for this launch. Port and
  token come from one JNI call, `Core.start(dataDir, configJson)`, which boots
  engram's axum server inside the app process; `Core.stop()` ends it. That
  connection lives in memory and is never written to `ConnectionStore`. The
  network security config permits cleartext to loopback and to nothing else.

Nothing above `core` learns which mode is on. Screens, `Models.kt`, `Ask`,
the drainer, picture loading and the parity tests are untouched, because all
of them already go through `transport()` and the contained core speaks the
same API. The two modes cannot drift apart: they are the same code.

Each mode keeps its own state. Server mode's stays where it has always been
(`engram.db`, `files/outbox`, `files/connection`): an existing install's outbox
is the one thing on the phone that exists nowhere else, and it is not moved to
tidy a directory. Contained mode has `files/contained/`: its outbox, its
models, and the core's own directory, `core/`, holding `control.db` and the
base under `bases/`, in a file named for the tenant. Its read cache and its outbox rows are a Room file of
their own, `contained.db`. An owed write in one mode's outbox is never drained
into the other source. Switching back finds everything where it was left.

The core listens on a port of the kernel's choosing, so the origin differs at
every launch. What the app keeps per source — held reads, kept answers — is
keyed by the word `contained`, not by that origin.

On the Rust side the core is a `cdylib` target of this crate for
`aarch64-linux-android`, behind a Cargo feature `contained`. The feature is
additive: it brings in what the phone needs and takes nothing out. Whether
OIDC, MCP, the templates and the CLI are worth carving out of the library is
decided in step 3, against the measured size of the `.so`. The server build
is unchanged.

## 2. Inside the core

### Vectors

`SqliteVectors` implements `VectorStore` beside `QdrantVectors` and
`MemoryVectors`. Points, dense vectors, BM25 postings and context sets are
`vec_*` tables inside `engram.db`; ranking is sqlite-vec's
`vec_distance_cosine` over every row, exact, with payload filters as plain
SQL. sqlite-vec is linked statically into the SQLite that sqlx bundles and
registered as an auto-extension, so nothing is loaded from storage at
runtime. It is pre-1.0; the version is pinned exactly.

What Qdrant does on its side of the wire, this store does in Rust to the same
arithmetic: IDF over the sparse half, reciprocal rank fusion of the two
halves, recency decay and the pinned boost. A base ranks the same in either.

One conformance suite, `src/vector/conformance.rs`, states what every store
must do, and runs against `MemoryVectors` and `SqliteVectors`.

### Inference

`infer/local.rs` implements `Embedder`, `Reranker`, `Synthesizer`/`Completer`
and `Transcriber` in process: llama.cpp through the `llama-cpp-2` binding,
whisper.cpp for speech, CPU only. Both embed ggml, and two copies in one
library collide, so ggml is built once and both are pointed at it. Their
pinned versions are therefore a matched pair and move together.

Configuration selects per role, as it does on the server: a role is `local`
with a model file, or an OpenAI-compatible `base_url` handled by the existing
`openai.rs`. Asking through an endpoint of the user's choosing is
configuration, not code. Prompts, embedding templates and budgets are the
server's.

Memory, on 8 GB: the embedder stays loaded while the app is in the
foreground. The reranker loads on first use and is released after a short
idle. The ask model loads for a question and the speech model for a
recording; each is dropped when done, or at once on a memory-pressure signal.
At most one large model is resident at a time.

### Models

A manifest in the app lists each model by name, URL, SHA-256, size and
licence. Downloads run in a foreground worker, resume, and are verified
before use. Files live in app-private storage. Changing a default is a
manifest change.

| Role | Default | Size | Licence | Fetched |
|---|---|---|---|---|
| Embedder | EmbeddingGemma 300M Q8_0, from the ungated `ggml-org/embeddinggemma-300M-GGUF` | 334 MB | Gemma terms, notice shown | first start |
| Reranker | a multilingual cross-encoder of 30–300M, chosen by measurement | ≤ 300 MB | per model | first start |
| Ask | Qwen3.5-2B Q4; Qwen3.5-4B as an explicit larger choice | ~1.3 GB; ~2.5 GB | Apache 2.0 | first visit to Ask |
| Speech | Whisper small, multilingual, Q5 | ~180 MB | MIT | first use of the microphone |

No reranker is listed until the device pass has chosen one. Every URL is
pinned to a revision, so a publisher's later upload cannot turn a good hash
into a failed download.

Gemma 4 E2B (Apache 2.0) is the alternative the ask default is measured
against, and takes the slot if Qwen3.5's Gated DeltaNet layers prove slow in
llama.cpp's ARM kernels.

**The reranker is the uncertain piece.** A cross-encoder reads query and
passage together for every candidate; thirty passages through a 0.6B model is
most of a minute on a phone. So: the top 10 only, passages truncated, only
for a deliberate search and for ask, never while typing, and the result lands
as a late refinement of an order already shown — the behaviour the server's
"where the reranker is consulted" setting already has. Candidates are
measured against `bge-reranker-v2-m3`'s ordering on a live base. If none
improves on no reranker within about two seconds on the device, contained
mode ships with the reranker off, and the first-start download is the
embedder alone.

The microphone door records `audio/wav`, so the transcriber reads PCM and
resamples to 16 kHz; there is no codec to carry.

## 3. Background work and the lifecycle

`Core.start` runs when something first needs the core: the app coming to the
foreground, or the drain worker after a share with no activity in front. There
is no permanent foreground service. The drain worker asks for no network in
contained mode, since loopback needs none. Writes go to the outbox first, as
now, and the drainer posts them to the loopback core; the core's job table
records unfinished jobs, which resume at the next start.

Three tiers:

1. **Immediate, always.** Extraction, chunking, embedding, indexing. No LLM.
   A capture is searchable seconds after it is made.
2. **Opportunistic.** Synthesis at ingest, consolidation and sleep, gaps,
   pairs, the tuning pass. One WorkManager job, constrained to charging, idle
   and battery not low, with a thermal check between jobs that ends the pass.
   It runs only where an endpoint is set, and then also wants an unmetered
   network. The model on the phone answers questions and nothing else until
   the device pass has measured whether it can read a capture; until then its
   work waits in the queue, held and not failed.
3. **Never.** MCP, the web interface, OIDC, tenants. Compiled out.

The core does not stop when the app leaves the foreground. Its models leave
memory when idle, and a job cut off with the process is reclaimed at the next
start.

The status endpoint reports which capabilities exist, as it does for speech.
Screens that depend on tier 2 show what there is and an empty state when
there is nothing: no banner, no badge. One line in Settings says what
background work is waiting and why.

A bulk import and a model download each run as a foreground worker with a
progress notification.

## 4. First start, switching, and what differs

### First start

`Nav.kt` shows `PairScreen` when no connection is stored. The condition
becomes "no mode chosen", and leads to a chooser with two choices:

- **On this phone.** Stores the choice and shows the download screen: the
  retrieval set with sizes and licence notices, one button, refused on a
  metered network unless confirmed. The app opens to Capture when the files
  verify.
- **With a server.** Today's `PairScreen`, unchanged. A pairing code arriving
  by scan or link still goes straight there.

An install that already holds a connection never sees the chooser and stays
in server mode. An update does not change what a working phone does.

The first visit to Ask in contained mode without a model offers three things
in place: download the ask model, set an endpoint, or leave ask off. The
first use of the microphone offers the speech model the same way once the
device can transcribe (step 2b); until then the microphone is absent in
contained mode, as it is against a server with no speech model. Until then
each door behaves as it does against a server that lacks the capability.

### Settings

"Server" becomes "Mode": the current mode and a switch. Switching to server
opens pairing, or reconnects if a connection is still stored; switching to
contained starts the core, or the download screen if the models are absent.
The confirmation says once that the two bases are separate and nothing is
copied. In contained mode the section also holds Models (installed, sizes,
remove, download), Ask (on-device, endpoint, off) and the background line.
"Unpair" remains, shown when a server connection exists.

There is no export of the contained base.

### What differs in contained mode

- **Reminders.** No server pushes them. The core computes what is due and the
  app schedules local notifications with AlarmManager. The "Reminders" and
  "Notifications" sections, which are about distributors and endpoints, are
  hidden.
- **The browser extension, MCP and the web interface** have no contained
  equivalent. They are what server mode remains for.

## 5. GrapheneOS

- The manifest declares `android:memtagMode="sync"`. The app runs tagged
  wherever tagging exists, and that is the only configuration tested.
- Nothing generates or loads code at runtime: the library ships in the APK,
  sqlite-vec and ggml are linked in, llama.cpp's CPU backend has no JIT. Both
  of GrapheneOS's dynamic code loading restrictions can be switched on for the
  app, and the device pass runs with them on.
- Model weights are memory-mapped files and are not tagged; only the KV cache
  and scratch buffers are tagged heap.

## 6. Build and tests

`cargo-ndk` builds the library for `arm64-v8a` with 16 KB page alignment; a
Gradle task runs it and places the result in `jniLibs`; the release workflow
gains the Rust Android target and the NDK. The APK grows by roughly
15–25 MB.

Tests, each where it is cheapest:

- **Desktop, this crate.** `SqliteVectors` against the shared suite. Local
  inference against tiny GGUF test models. One integration test that boots
  the `contained` feature set and runs ingest, search and ask against a fake
  synthesizer.
- **JVM, `android/core`.** Mode selection; that each mode's state stays in
  its own directory; that one mode's outbox never drains into the other; the
  manifest's download, resume and verify against a local HTTP server.
- **Device, once, at the end.** A Pixel 8 on GrapheneOS with tagging and both
  restrictions on: the core starts, the download, capture to searchable,
  rerank latency per candidate, ask tokens per second for Qwen3.5-2B and
  Gemma 4 E2B, Whisper latency on a short note, memory pressure with another
  app open, one idle-charging background pass. The numbers go in the commit
  that sets the defaults.

The risk, stated once: llama.cpp or whisper.cpp may trip memory tagging, and
Qwen3.5's hybrid layers may be slow on the G3. Either is answered by a
version pin or a manifest change, and neither reaches the architecture.

## 7. Order

Each step merges on its own.

1. `SqliteVectors`, the `contained` feature, the desktop end-to-end test.
2. Local inference over the shared ggml: embedder, reranker, ask, transcriber.
3. The `cdylib`, `Core.start`/`Core.stop`, the Gradle and NDK build.
4. The app: mode storage, the loopback connection, per-mode state, the choice
   in `Engram.kt`.
5a. The manifest, the downloader and its worker, an ask endpoint the core can
    be given, the platform certificate verifier.
5b. The first-start chooser, the download screen, Settings "Mode", the offer
    on the first visit to Ask.
6. The background pass and local reminders.
7. The device pass; reranker and ask defaults set from what it measures.

## Left out

- **The TPU.** GrapheneOS ships the Edge TPU's NNAPI driver, so an app can
  reach it, but NNAPI is deprecated, takes only int8 models of fixed shape,
  and Google's replacement for Tensor is an experimental SDK behind a
  sign-up. It cannot run a decoder. The embedder is the one candidate and is
  not the bottleneck. A TPU-backed `Embedder` is a later spike behind the
  existing trait.
- **Moving a base between modes**, in either direction.
- **An approximate index.** Worth revisiting past roughly 100k passages.
- **Any architecture but `arm64-v8a`**, beyond what an emulator in CI needs.
