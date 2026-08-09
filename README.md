<img src="assets/wordmark.svg" alt="engram" width="260">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Capture text, and engram
splits it into self-contained markdown chunks, embeds them, and answers queries
with ranked excerpts — no generation step in the way. Synthesis is a separate
endpoint you ask for explicitly.

Three front doors over one backend: a web UI, a REST API, and an MCP server, so
Claude Code or Claude Desktop can read and write it mid-session.

Design notes: [docs/superpowers/specs/2026-08-09-engram-design.md](docs/superpowers/specs/2026-08-09-engram-design.md).
Planned work: [ROADMAP.md](ROADMAP.md).

## Requirements

- Rust 1.94+ (the floor comes from sqlx 0.9).
- Qdrant over its REST API — 6333 locally, or whatever a proxy serves it on.
  No gRPC port needed.
- An OpenAI-compatible endpoint for chat completions and embeddings. One server
  can fill all roles, or point each role somewhere different.

## Quick start

```bash
docker compose up -d                             # Qdrant on 6333
cp config.example.toml config.toml
cargo run -- --hash-password 'your password'     # paste the hash into config.toml
cargo run
```

Open <http://127.0.0.1:8080/auth/login>, capture something, and watch it move
through `raw → segmenting → embedding → ready` on Browse. `partial` means some
chunks failed to embed; the Ops screen lists the failures with a retry button.

## Configuration

Any key can be set by environment variable: prefix `ENGRAM__`, `__` between
levels, e.g. `ENGRAM__INFER__EMBED__DIM=768`. Put secrets there rather than in
the file — the loader warns if it finds one. `--print-config` prints the
effective config with secrets redacted.

| Key | Meaning |
|---|---|
| `server.bind` | Listen address. Default `127.0.0.1:8080`. |
| `server.workers` | Background job workers. One is right for a single local GPU. |
| `store.path` | SQLite file. **Back this up.** |
| `vector.url` | Qdrant base URL, e.g. `http://localhost:6333`. |
| `vector.collection` | Alias name. Data lives in `{name}_v1`, `_v2`, … |
| `vector.api_key` | Prefer `ENGRAM__VECTOR__API_KEY`. |
| `vector.recency_weight` | How much age counts against a result. `0.0` disables. Default 0.05. |
| `vector.recency_half_life_days` | Age at which half that boost is gone. Default 180. |
| `vector.pinned_boost` | Extra score for a chunk tagged `pinned`. Default 0.15. |
| `infer.chunk.*` | Segmentation model: `base_url`, `model`, `context_tokens`, `max_output_tokens`, `output_ratio`, optional `tokenizer_path`, `timeout_secs`, `reasoning_effort`, `cooldown_secs`. |
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`, `timeout_secs`. |
| `infer.ask.*` | Completion model, used only by `ask`. Same timeout and reasoning keys. |
| `infer.rerank.*` | Optional. `style` is `tei`, `cohere` or `vllm`. Off by default. |
| `auth.mode` | `oidc` or `local`. |
| `auth.oidc.*` | `issuer_url`, `client_id`, `client_secret`, `redirect_url`, `scopes`, `allowed_subs` / `allowed_emails`. |
| `auth.local.*` | `username` and an argon2id `password_hash`. Development only. |

Three worth knowing:

- **`infer.embed.dim`** must match the collection. If it does not, engram refuses
  to start and names both numbers. Mismatched vectors corrupt search in a way you
  would not notice for weeks.
- **`infer.chunk.output_ratio`** — the segmenter rewrites chunks to be
  self-contained, so its output can be larger than its input. It is also what
  sizes the input window: `min(context / (1 + ratio), max_output / ratio)`.
  Raise it if segmentation responses get truncated; the default of 8.0 gives
  ~2000 tokens of input per call, which a 9B model can rewrite without running
  out of room.
- **`infer.embed.max_input_tokens`** must be the *server's* ceiling, not the
  model's nominal one. llama.cpp refuses any input above its physical batch
  size — often 1024 — with a 500 that no retry can fix. engram splits a refused
  chunk rather than retrying it, but it splits sooner and cheaper when this
  number is honest.
- **`timeout_secs`** defaults to 900. That is absurd for a hosted API and about
  right for a local model — see below.

## Running against a small local model

This is the case engram is built for, and it behaves differently enough from a
hosted API to be worth stating.

A segmentation window against a 9B model on one consumer GPU has been measured
at seven minutes and 8000 output tokens. Three consequences shape the defaults:

- **Timeouts must outlast the model.** A timeout is indistinguishable from a
  dead endpoint to the job runner: the call fails, the job retries, and it
  fails again at the same wall. Hence 900 seconds by default, and per role so a
  fast embedder is not held to the segmenter's patience.
- **Reasoning tokens come out of the output budget.** A reasoning model thinks
  before it writes any JSON, and that spending is taken from the same
  `max_output_tokens` the chunk list has to fit in. Set
  `reasoning_effort = "none"` if your endpoint honours it, and keep
  `max_output_tokens` generous. The field is unset by default because models
  that do not reason reject it.
- **A truncated answer is normal, not exceptional.** When the chunk list is cut
  off mid-object, the chunks that did finish are kept rather than the window
  being retried — asking a slow model to do it again is the most expensive
  thing engram can do.

Two knobs exist for the hardware rather than the output. `cooldown_secs` idles
between windows so a long source is not one unbroken thermal load — it saves no
energy, since the same tokens are generated either way, but it lets the card
settle. And `workers = 1` is right when every role points at one GPU: a second
worker does not double throughput, because the inference server serialises the
calls regardless.

What does save energy is generating fewer tokens: `reasoning_effort = "none"`
where the endpoint honours it, and not re-segmenting work that already
succeeded — which is what the per-window resume is for.

Segmentation logs each call's duration and token count, because minutes of
silence otherwise looks exactly like a hang. Browse shows `segmenting 3/9`
for the same reason.

## Inference roles

`chunk`, `embed` and `ask` are configured separately and can point at different
servers. Reranking is opt-in because there is no OpenAI-standard rerank endpoint,
so its wire format is configured rather than guessed.

Ingest never calls inference. Capture writes the text and returns; segmentation
and embedding run in the background with retries. A dead endpoint slows
processing but never loses a capture.

Segmentation runs one window at a time and remembers where it got to. A window
that succeeds is written before the next is attempted, so a retry resumes
rather than re-paying for the windows that already worked, and a window the
chunker cannot handle is split structurally on its own lines while the rest
keep their LLM segmentation. That source is reported `partial`, and Ops names
the window. Browse shows `segmenting 3/9` while it happens.

Paste a chapter at a time. A book works — it is windowed the same way — but it
costs one model call per window, and a chapter is what search results read best
from. The capture screen says so and warns above roughly one window's worth of
text. The request body limit is an explicit 8 MB.

## Does the chunk still say what the source said?

Each window is checked before its chunks are stored.

- **Literals.** Commands, paths and flags in a chunk must appear in the window
  it came from, compared with whitespace normalised. If they do not, the window
  is segmented once more; failing that, the chunk is stored with a flag naming
  the literal that went missing. A paraphrased command is a command that later
  gets pasted into a root shell, and losing the chapter to protect against that
  would be worse than a warning the reader can see.
- **Spans.** A chunk's claimed `source_lines` are clamped to its own window and
  checked for plausible overlap with the lines they name. The detail pane
  renders those lines beside the chunk, so a wrong span is not cosmetic. Models
  omit `source_lines` more often than not; when that happens the span is
  recovered by finding the chunk's own verbatim lines in the window, and only a
  span the model actually asserted is ever doubted.
- **Coverage.** The fraction of a source's lines that ended up inside some
  chunk is recorded and shown on Browse. Below 60% it reads as a warning — a
  source where the segmenter dropped half a chapter used to look identical to
  one where it did not.

Flagged chunks are listed on Ops with two actions: re-segment that one window,
or mark the chunk reviewed.

## How search works

One embedding call, one hybrid query, and one rerank call if a reranker is
configured. Never a completion.

**Hybrid** = two branches fused inside Qdrant in a single round trip: the dense
vector from the embedding model, and a BM25 sparse vector computed locally from
the same text. Dense embeddings blur exact tokens, and this kind of knowledge
base is full of `E01`, `--dry-run`, `/etc/fstab` and error strings. The two
rankings are merged with reciprocal rank fusion, which needs no score
calibration between a cosine similarity and a term weight.

Tokenisation keeps `-`, `_`, `.` and `/` inside terms and also emits the pieces,
so `--dry-run` matches `dry-run`, `dry` or `run`. Qdrant supplies the inverse
document frequency. A query with no indexable term skips the lexical branch.

**One source leads with at most three chunks**, so a forty-chunk document
cannot take the top of the list from the rest of the corpus. What that displaces
goes back on the end in rank order, so the cap reorders a result list rather
than shortening it — a base holding a single document still answers with as much
as it has. `ask` opts out of the reordering entirely: an answer often lives in
one document, and it reads better in rank order.

**Recency and pinning** are applied as a final scoring pass. The weights let
recency break a near-tie but never overturn a clearly better match. Pinning is a
tag: `PATCH /api/v1/chunks/{id}` with `{"tags": ["pinned"]}`.

**`GET /api/v1/resurface`** returns a random handful of chunks older than a month
that have not appeared in results since. Every search records what it showed, so
surfacing something counts as remembering it.

Result **scores are ranking scores, not similarities**. A hybrid query returns a
fused rank, a query with no indexable term returns a cosine, and both then carry
the recency term. They order one result list and mean nothing between two, which
is why the UI shows a position rather than a number.

The search page keeps the ranked list beside the result. Opening a hit fills a
detail pane with the chunk and the source lines it claims, so a paraphrase is
visible without leaving the page; `/ui/chunks/{id}` is the same view as a
standalone page, for links and new tabs. Query terms are highlighted, long
chunks clamp with an expand control, and every fenced block has a copy button.

Typing is cheap: query embeddings are cached, so a burst of keystrokes costs one
embedding call rather than one per prefix. Incremental searches do not record
what they showed — only opening a chunk, or a deliberate API, MCP or `ask` call,
does. That is what keeps `resurface` meaningful.

Editing a chunk's `tags` or `category` rewrites the Qdrant payload in place.
Editing `text` or `title` queues a re-embed — those are what the model was shown.
An absent field in a `PATCH` is left alone; an explicit `null` clears it.

An edit that lands while a chunk is being embedded wins: chunks carry a revision
that vector-invalidating edits bump, and the in-flight job's "indexed" mark only
applies while the revision still matches. A losing job leaves the chunk pending,
which is what gets it embedded again from the text that is actually there.

## Collection generations

`vector.collection` is a Qdrant **alias**. The vectors live in `chunks_v1`,
`chunks_v2`, … and the alias points at the current one, so a schema change is a
background rebuild plus an atomic swap instead of an outage:

```bash
cargo run -- --reindex
```

Points are copied into the next generation and the alias moves onto it. Dense
vectors are copied as-is, so this costs no embedding calls. The previous
generation is left in place — it is the only rollback there is.

A collection created before this layout shares its name with the alias, and
Qdrant will not allow both. Freeing the name means deleting the source after its
points are copied and counted, so that case needs `--reindex --replace-legacy`.

A rebuild cannot change vector width. See below for that.

## Changing the embedding model

1. Update `infer.embed.model` and `infer.embed.dim`.
2. Point `vector.collection` at a new alias name, or delete the existing
   generations.
3. Start engram; it creates a fresh generation at the new dimension.
4. Re-embed: `POST /api/v1/sources/{id}/reprocess` with `{"stage":"embed"}`.

Skipping step 2 is refused at startup rather than silently accepted.

## Connecting Claude Code

Mint a token on the Ops screen (shown once, stored as an argon2id hash), then:

```bash
claude mcp add --transport http engram http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer engram_..."
```

Three tools appear: `ingest`, `search` and `ask`. They return markdown.

## Auth

**OIDC** for production: authorization code flow with PKCE, server-side sessions
in SQLite so sign-out actually revokes, and a mandatory allowlist — leaving both
`allowed_subs` and `allowed_emails` empty denies everyone rather than admitting
every account your provider knows.

**Local mode** is a single hardcoded credential for development. It refuses a
non-loopback bind without `--i-know-this-is-insecure`.

## Security posture

- Chunk text is model output rendered into an authenticated session, so it is
  treated as untrusted: rendered markdown is sanitized with `ammonia` and the
  URL scheme allowlist is explicit. The one `|safe` interpolation in the
  templates is the already-sanitized output.
- API tokens are argon2id hashed and shown exactly once. Sessions are
  server-side rows, so signing out actually revokes.
- Local auth mode refuses a non-loopback bind without an explicit override
  flag.
- Internal errors (SQL, prompt fragments) never reach clients; they go to the
  log and the client sees a generic message. Qdrant error bodies are reduced to
  their message and truncated before they reach a log line.
- Chunk metadata is bounded on the way in: a chunk carries at most 32 tags of
  64 characters, because tags become payload on every point and a keyword index
  in Qdrant.
- CI runs `cargo audit` on every push. Advisories are ignored one id at a time
  in `.cargo/audit.toml`, each with a written reachability argument — currently
  one entry, RUSTSEC-2023-0071 in `rsa` via `openidconnect`, which concerns
  private-key operations that engram never performs.

## Backup

SQLite is the source of truth; it holds every raw capture verbatim.

- **Full:** copy `engram.db` (plus `-wal`/`-shm`) and the Qdrant volume.
- **Minimal:** copy `engram.db` alone. Vectors can be regenerated by
  reprocessing, at the cost of re-running embeddings.

## Development

```bash
cargo test                                         # no containers needed
cargo test --test integration_qdrant -- --ignored  # needs a running Qdrant
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Set `ENGRAM_TEST_QDRANT` if Qdrant is not on `localhost:6333`. Everything except
the Qdrant suite runs without infrastructure: inference and the vector store sit
behind traits, and the tests inject fakes plus an in-memory vector store.
