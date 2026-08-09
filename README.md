# pkdb

A self-hosted personal knowledge base. Drop in text, get it back later by
meaning rather than by keyword. Retrieval is a search problem: the default path
embeds your query, searches vectors, and returns ranked markdown chunks with no
generation step. See [the design](docs/superpowers/specs/2026-08-09-pkdb-design.md)
for why it is built this way.

Three front doors over one backend: a web UI, a REST API, and an MCP server so
Claude Code or Claude Desktop can read and write the base during a session.

## Requirements

- Rust 1.88 or newer (uses let-chains).
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

Every key can be overridden by an environment variable using the `PKDB__`
prefix and `__` as the nesting separator, e.g. `PKDB__INFER__EMBED__DIM=768`.
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
| `vector.api_key` | Optional; prefer `PKDB__VECTOR__API_KEY`. |
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
than its input. The ratio tells pkdb how much larger to assume when sizing input
windows. Raise it if segmentation responses are being truncated.

**`infer.embed.dim`** must match the Qdrant collection. If it does not, pkdb
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
claude mcp add --transport http pkdb http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer pkdb_..."
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

- **Full backup:** copy `pkdb.db` (plus `-wal`/`-shm` if present) and the Qdrant
  volume.
- **Minimal backup:** copy `pkdb.db` alone. Vectors can be regenerated by
  reprocessing, at the cost of re-running embeddings.

## Changing the embedding model

Vector dimensions are model-specific, so old vectors become meaningless.

1. Update `infer.embed.model` and `infer.embed.dim`.
2. Delete the Qdrant collection (it is a cache, not the source of truth).
3. Start pkdb; it recreates the collection at the new dimension.
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

Set `PKDB_TEST_QDRANT` if Qdrant is not on `localhost:6334`.

Everything except the Qdrant suite runs without infrastructure: the inference
roles and the vector store sit behind traits, and the tests inject deterministic
fakes plus an in-memory brute-force vector store.

## Not built (yet)

Hybrid keyword + vector search (the FTS5 index and its triggers exist and are
tested, but the retrieval path does not use them), reranking on by default, a
CLI, retrieval-quality evaluation, OAuth 2.1 for `/mcp`, and file upload.
