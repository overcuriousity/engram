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
| `server.workers` | Background job workers. Default 2. |
| `store.path` | SQLite file. **Back this up.** |
| `vector.url` | Qdrant base URL, e.g. `http://localhost:6333`. |
| `vector.collection` | Alias name. Data lives in `{name}_v1`, `_v2`, … |
| `vector.api_key` | Prefer `ENGRAM__VECTOR__API_KEY`. |
| `vector.recency_weight` | How much age counts against a result. `0.0` disables. Default 0.05. |
| `vector.recency_half_life_days` | Age at which half that boost is gone. Default 180. |
| `vector.pinned_boost` | Extra score for a chunk tagged `pinned`. Default 0.15. |
| `infer.chunk.*` | Segmentation model: `base_url`, `model`, `context_tokens`, `max_output_tokens`, `output_ratio`, optional `tokenizer_path`. |
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`. |
| `infer.ask.*` | Completion model, used only by `ask`. |
| `infer.rerank.*` | Optional. `style` is `tei`, `cohere` or `vllm`. Off by default. |
| `auth.mode` | `oidc` or `local`. |
| `auth.oidc.*` | `issuer_url`, `client_id`, `client_secret`, `redirect_url`, `scopes`, `allowed_subs` / `allowed_emails`. |
| `auth.local.*` | `username` and an argon2id `password_hash`. Development only. |

Two worth knowing:

- **`infer.embed.dim`** must match the collection. If it does not, engram refuses
  to start and names both numbers. Mismatched vectors corrupt search in a way you
  would not notice for weeks.
- **`infer.chunk.output_ratio`** — the segmenter rewrites chunks to be
  self-contained, so its output can be larger than its input. Raise this if
  segmentation responses get truncated.

## Inference roles

`chunk`, `embed` and `ask` are configured separately and can point at different
servers. Reranking is opt-in because there is no OpenAI-standard rerank endpoint,
so its wire format is configured rather than guessed.

Ingest never calls inference. Capture writes the text and returns; segmentation
and embedding run in the background with retries. A dead endpoint slows
processing but never loses a capture.

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
