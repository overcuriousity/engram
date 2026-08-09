<img src="assets/wordmark.svg" alt="engram" width="260">

**A trace of everything worth keeping.**

An *engram* is the physical trace a memory leaves in neural tissue — the
hypothesised substrate of recall. This is one you own: a self-hosted store for
discrete, reusable knowledge, retrieved by meaning rather than by keyword.

Retrieval is a search problem, not a chat problem. The default path embeds your
query, searches vectors, and returns ranked markdown chunks. No generation step,
no waiting on a model to finish thinking. Synthesis is available when you
actually want it, on a separate page, never by accident.

Three front doors over one backend: a web UI, a REST API, and an MCP server, so
Claude Code or Claude Desktop can read and write your knowledge base mid-session
— turning "I just explained this to an AI" into something permanent.

See [the design](docs/superpowers/specs/2026-08-09-engram-design.md) for why it
is built this way.

## The mark

The logo is the mechanism, drawn to scale: a query point in latent space, bound
to its nearest neighbours, with more distant points present in the space but
absent from the answer. Read another way, it is a neuron — soma, dendrites, and
the field it listens to.

## Requirements

- Rust 1.94 or newer. The floor comes from sqlx 0.9; our own code needs
  only 1.88 for let-chains.
- Qdrant, reachable over **gRPC** (port 6334, not just the REST port 6333).
- An OpenAI-compatible inference endpoint providing chat completions and
  embeddings. One server can fill all roles, or you can point each role
  somewhere different.

## Quick start

```bash
docker compose up -d                  # starts Qdrant on 6333 (REST) and 6334 (gRPC)
cp config.example.toml config.toml
cargo run -- --hash-password 'your password'   # paste the hash into config.toml
cargo run
```

Then open <http://127.0.0.1:8080/auth/login>.

Capture something, and watch it move through `raw → segmenting → embedding →
ready` on the Browse screen. A source whose chunks partly failed to embed shows
as `partial`; the Ops screen lists the failed jobs with their errors and a retry
button.

## Configuration

Every key can be overridden by an environment variable using the `ENGRAM__`
prefix and `__` as the nesting separator, e.g. `ENGRAM__INFER__EMBED__DIM=768`.
Secrets belong in the environment; the loader warns at startup if it finds one
in the file. `cargo run -- --print-config` prints the effective configuration
with secrets redacted.

| Key | Meaning |
|---|---|
| `server.bind` | Listen address. Default `127.0.0.1:8080`. |
| `server.workers` | Background worker tasks draining the job queue. Default 2. |
| `store.path` | SQLite file. Created on first run. **Back this up.** |
| `vector.url` | Qdrant gRPC endpoint, e.g. `http://localhost:6334`. |
| `vector.collection` | Collection name. |
| `vector.api_key` | Optional; prefer `ENGRAM__VECTOR__API_KEY`. |
| `infer.chunk.*` | Segmentation model: `base_url`, `model`, `context_tokens`, `max_output_tokens`, `output_ratio`, optional `tokenizer_path`. |
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`. |
| `infer.ask.*` | Completion model used only by `ask`. |
| `infer.rerank.*` | Optional. `style` is `tei`, `cohere` or `vllm`. Disabled by default. |
| `auth.mode` | `oidc` or `local`. |
| `auth.oidc.*` | `issuer_url`, `client_id`, `client_secret`, `redirect_url`, `scopes`, and **`allowed_subs` / `allowed_emails`**. |
| `auth.local.*` | `username` and an argon2id `password_hash`. Development only. |

Two settings deserve attention.

**`infer.chunk.output_ratio`** exists because the segmenter rewrites chunks to
be self-contained rather than merely splitting them, so its output can be larger
than its input. The ratio tells engram how much larger to assume when sizing input
windows. Raise it if segmentation responses are being truncated.

**`infer.embed.dim`** must match the Qdrant collection. If it does not, engram
refuses to start and names both numbers, because writing mismatched vectors
corrupts search results in a way you would not notice for weeks.

## The three inference roles

`chunk`, `embed` and `ask` are configured independently and can point at
different servers. Swapping one does not affect the others. There is no
OpenAI-standard rerank endpoint, so reranking is opt-in and its wire format is
configured explicitly rather than guessed.

Ingest never calls inference. Capture writes the text and returns; segmentation
and embedding happen in the background and retry with exponential backoff. A
dead inference endpoint slows processing but never loses a capture.

## Connecting Claude Code

Mint a token on the Ops screen (shown once, stored only as an argon2id hash),
then:

```bash
claude mcp add --transport http engram http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer engram_..."
```

Three tools appear: `ingest`, `search` and `ask`. They return markdown, which is
what an agent wants to read.

## Auth

**OIDC** is the production mode: authorization code flow with PKCE, server-side
sessions in SQLite so revocation works, and an allowlist. The allowlist is
mandatory — leaving both `allowed_subs` and `allowed_emails` empty denies
everyone rather than admitting every account your identity provider knows about.

**Local mode** is a development shortcut with a single hardcoded credential. It
refuses to bind to a non-loopback address unless you pass
`--i-know-this-is-insecure`, because a dev shortcut reachable from the network
is production auth by accident.

## Backup

The SQLite file is the source of truth: it holds every raw capture verbatim.
Copy it and you can rebuild everything else.

- **Full backup:** copy `engram.db` (plus `-wal`/`-shm` if present) and the Qdrant
  volume.
- **Minimal backup:** copy `engram.db` alone. Vectors can be regenerated by
  reprocessing, at the cost of re-running embeddings.

## Changing the embedding model

Vector dimensions are model-specific, so old vectors become meaningless.

1. Update `infer.embed.model` and `infer.embed.dim`.
2. Delete the Qdrant collection (it is a cache, not the source of truth).
3. Start engram; it recreates the collection at the new dimension.
4. Re-embed every source:
   `POST /api/v1/sources/{id}/reprocess` with `{"stage":"embed"}`.

Skipping step 2 is refused at startup rather than silently accepted.

## Development

```bash
cargo test                                       # unit and HTTP tests, no containers
cargo test --test integration_qdrant -- --ignored  # needs a running Qdrant
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Set `ENGRAM_TEST_QDRANT` if Qdrant is not on `localhost:6334`.

Everything except the Qdrant suite runs without infrastructure: the inference
roles and the vector store sit behind traits, and the tests inject deterministic
fakes plus an in-memory brute-force vector store.

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
  log and the client sees a generic message.
- CI runs `cargo audit` on every push. Advisories are ignored one id at a time
  in `.cargo/audit.toml`, each with a written reachability argument — currently
  one entry, RUSTSEC-2023-0071 in `rsa` via `openidconnect`, which concerns
  private-key operations that engram never performs.

## Not built (yet)

Hybrid keyword + vector search (the FTS5 index and its triggers exist and are
tested, but the retrieval path does not use them), reranking on by default, a
CLI, retrieval-quality evaluation, OAuth 2.1 for `/mcp`, and file upload.
