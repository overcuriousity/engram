# pkdb — Design

Date: 2026-08-09
Status: approved
Supersedes nothing. Implements the concept in `spec.md`.

## 1. Scope

pkdb is a self-hosted personal knowledge base. It stores discrete, reusable
pieces of knowledge and retrieves them by meaning. Retrieval is a search
problem; generation is an optional layer, never the default path.

This document fixes the technical design for v1. It assumes `spec.md` for
purpose and principles and does not repeat them.

### Decisions

| Area | Decision |
|---|---|
| Language | Rust |
| HTTP | axum |
| Metadata store | SQLite (WAL) via `sqlx` |
| Vector store | Qdrant |
| Job queue | `jobs` table in SQLite, in-process tokio workers |
| Web UI | Server-rendered Askama templates + htmx, no node toolchain |
| Chunking | LLM rewrites each chunk to be self-contained |
| Chunk format | Markdown |
| Inference | Three independently configured roles: chunk, embed, rerank |
| Rerank | Optional, disabled by default in v1 |
| Auth | OIDC (authorization code + PKCE); local username/password for dev |
| Machine auth | Bearer API tokens minted in the web UI |
| MCP transport | Streamable HTTP at `/mcp`, bearer authenticated |
| Deployment | One binary + one SQLite file + one Qdrant container |

## 2. Architecture

```
                    pkdb (single binary, tokio)
  ┌──────────────────────────────────────────────────────┐
  │  axum HTTP server                                    │
  │   ├─ /ui/*      htmx + Askama templates (session)    │
  │   ├─ /api/v1/*  REST (bearer token or session)       │
  │   ├─ /auth/*    OIDC code+PKCE / dev local login     │
  │   └─ /mcp       MCP over streamable HTTP (bearer)    │
  ├──────────────────────────────────────────────────────┤
  │  core::  ingest() search() ask() browse()            │
  ├──────────────────────────────────────────────────────┤
  │  worker pool (N tokio tasks, poll jobs table)        │
  └───────┬─────────────────┬─────────────────┬──────────┘
          │                 │                 │
      SQLite (WAL)      Qdrant (HTTP)   Inference (reqwest)
      sources, chunks,  vectors +       chunk / embed / rerank
      jobs, sessions,   payload
      tokens
```

Every front door calls `core`. No logic is duplicated per interface.

### Modules

| Module | Responsibility |
|---|---|
| `core` | `ingest` / `search` / `ask` / `browse`. Pure logic, no HTTP types. |
| `store` | SQLite via `sqlx`. Embedded migrations. |
| `vector` | Qdrant client wrapper: collection setup, upsert, filtered search. |
| `infer` | `Chunker`, `Embedder`, `Reranker` traits; each bound to a base URL + model. |
| `jobs` | Claim / complete / retry loop, stage state machine. |
| `web` | axum routers, Askama templates, htmx fragments. |
| `mcp` | `rmcp` server exposing `ingest`, `search`, `ask` as thin wrappers over `core`. |
| `auth` | OIDC flow, session cookies, API tokens, dev local mode. |

### Why MCP over HTTP, not stdio

Stdio would require a local process with direct database access, which breaks
the single-backend rule. Over HTTP, agent clients point at `https://<host>/mcp`
with a bearer token, exactly like the future CLI.

### Deliberately excluded from v1

No Redis. No separate worker process. Both can be added later without changing
`core`.

## 3. Data model

### SQLite

```sql
sources
  id            TEXT PK        -- uuidv7 (time-sortable)
  raw_text      TEXT NOT NULL  -- verbatim, never mutated
  origin        TEXT           -- 'web' | 'mcp' | 'cli'
  title_hint    TEXT           -- optional user label at capture
  content_hash  TEXT NOT NULL  -- sha256(raw_text), UNIQUE
  status        TEXT NOT NULL  -- raw|segmenting|segmented|embedding|ready|partial|failed
  created_at    INTEGER
  updated_at    INTEGER

chunks
  id            TEXT PK
  source_id     TEXT REFERENCES sources(id) ON DELETE CASCADE
  ordinal       INTEGER        -- position within source
  text          TEXT NOT NULL  -- LLM-rewritten, self-contained, markdown
  source_span   TEXT           -- JSON {start_line,end_line}, best-effort pointer into raw_text
  title         TEXT
  category      TEXT
  tags          TEXT           -- JSON array
  embed_state   TEXT           -- pending|embedded|failed
  embed_model   TEXT           -- model that produced the live vector
  created_at    INTEGER

chunks_fts      -- FTS5 virtual table over (text, title, tags), synced by triggers

jobs
  id            INTEGER PK
  stage         TEXT           -- segment|enrich|embed
  target_kind   TEXT           -- source|chunk
  target_id     TEXT
  state         TEXT           -- pending|running|done|failed
  attempts      INTEGER DEFAULT 0
  run_after     INTEGER        -- backoff timestamp
  last_error    TEXT
  claimed_at    INTEGER
  UNIQUE(stage, target_id)

sessions        id, subject, email, expires_at, created_at
api_tokens      id, name, token_hash, subject, created_at, last_used_at, revoked_at
```

### Qdrant

Collection `chunks`, one vector per chunk. Payload:

```
chunk_id, source_id, text, title, category, tags[], created_at
```

The payload carries `text` so search returns results without a SQLite round
trip. Payload indexes on `tags`, `category`, `created_at` support filtered
search.

### Rationale for three non-obvious columns

- **`content_hash` UNIQUE** — re-pasting the same procedure returns the
  existing source instead of duplicating it.
- **FTS5 alongside vectors** — about twenty lines of triggers, and it buys
  exact-string recall for error codes, CLI flags, and registry paths, which
  embeddings handle poorly. Search stays pure-vector in v1; hybrid search can
  be added later without a migration.
- **`embed_model` per chunk** — swapping embedding models changes vector
  dimensions and invalidates old vectors. This column makes "re-embed
  everything not on model X" a single query, which is what makes reprocessing
  without re-ingesting possible.

## 4. Ingest pipeline

`POST /api/v1/sources` hashes the text, checks for a duplicate, inserts a
`sources` row with status `raw`, enqueues a `segment` job, and returns
`{id, status}`. No inference call happens in the request path.

### Segment job (per source)

1. If `raw_text` exceeds the chunker's input budget, pre-split on structural
   boundaries — markdown headings, then blank lines, then a hard line cap —
   into windows with one heading of overlap. Each window is sent separately and
   results are concatenated with continuous `ordinal` values.
2. The prompt requests JSON:

   ```json
   {"chunks":[{"text":"...","title":"...","category":"...",
               "tags":["..."],"source_lines":[12,28]}]}
   ```

   Each chunk is one atomic unit: one technique, one procedure, one fact. The
   model resolves pronouns and implicit references into the rewritten text
   ("this command" becomes the actual command), and must reproduce commands,
   paths, error strings, and code verbatim rather than paraphrasing them. That
   instruction is the guardrail against paraphrase drift on technical content.
3. Parse the response. On malformed JSON, retry once with the parse error
   appended to the prompt. On a second failure, fall back to structural-split
   chunks with no rewriting and mark the source `partial`. A non-empty source
   never yields zero chunks.
4. Insert chunks, enqueue one `embed` job per chunk, set the source to
   `embedding`.

Enrichment is folded into segmentation, since the same call already returns
title, category, and tags. The `enrich` stage remains in the job enum so
re-tagging can be added later without a schema change.

### Embed job (per chunk)

Embed `title + "\n" + text`, since the title carries topical signal. Upsert to
Qdrant using `chunk_id` as the point id, then set `embed_state` and
`embed_model`. When the last chunk succeeds the source becomes `ready`; if any
chunk exhausts its attempts the source becomes `partial`.

### Retries

Five attempts with exponential backoff — 2s, 4s, 8s, and so on, capped at five
minutes — scheduled through `run_after`. On worker startup, `running` rows
older than a timeout are reclaimed, which recovers work interrupted by a crash.

## 5. Context budgeting

Configuration states real limits rather than assuming them:

```toml
[infer.chunk]
base_url = "..."
model = "..."
context_tokens    = 32768
max_output_tokens = 8192
output_ratio      = 1.4    # rewriting can exceed input length

[infer.embed]
model = "..."
dim = 1024
max_input_tokens = 8192

[infer.ask]
context_tokens = 32768
```

Token counting uses the `tokenizers` crate with the model's `tokenizer.json`
when available, and a conservative `chars / 3.5` estimate otherwise. The active
method is logged at startup so the fallback is never silent.

**Segment window sizing.** Because the chunker rewrites rather than splits,
output can exceed input. Usable input per call:

```
window = (context_tokens - prompt_overhead) / (1 + output_ratio)
```

`prompt_overhead` is measured at startup by tokenizing the rendered chunker
system prompt, not guessed. For a 32k model the resulting window is roughly 40%
of the context. A source too large for one window becomes several windows; it
is never clipped.

**Chunk size ceiling.** The prompt instructs chunks to stay under
`embed.max_input_tokens * 0.8`. If a chunk still overflows, the embed job
splits it at a paragraph boundary into sibling chunks with sequential ordinals
rather than truncating. This avoids multiple vectors per chunk and loses no
text.

**Ask budgeting.** Retrieved chunks are packed into the prompt highest score
first until the budget is exhausted. The response reports which chunks were
included, so a dropped citation is visible.

## 6. Markdown as the chunk format

Chunk `text` is markdown by contract, because it is written by an LLM and read
by LLMs.

- The chunker prompt requires markdown: fenced code blocks with a language tag,
  lists for procedures, tables where they fit. H1 is forbidden — `title` is a
  separate field, so chunks start at H2 and nest cleanly when composed.
- The web UI renders with `pulldown-cmark` and then **sanitizes with
  `ammonia`**. LLM-generated markdown can contain raw HTML; rendering it
  unsanitized is a stored-XSS path into the operator's own session. This is not
  optional.
- REST and MCP return raw markdown, unrendered, which is what an agent wants.
- Embedding sends the markdown verbatim. Backticks and fences around commands
  are signal; stripping them would separate code from its context.
- FTS5 indexes the markdown as-is, so searching for `--force` or a file path
  still matches.

## 7. Retrieval pipeline

`GET /api/v1/search?q=&limit=&tags=&category=`

1. Embed the query (one call).
2. Search Qdrant for `limit * 3` candidates with payload filters applied
   server-side.
3. If rerank is configured and enabled, rerank the candidates and take the top
   `limit`. Disabled by default in v1.
4. Return `[{chunk_id, source_id, title, text, category, tags, score}]`. This
   is the answer; no completion call is involved.

`POST /api/v1/ask` performs the same retrieval, then makes one chat completion
over the retrieved chunks and returns the answer with its citations. It is a
separate endpoint on a separate UI page and is never the default path.

## 8. Authentication

`auth.mode` is either `oidc` or `local`. Exactly one is active.

### OIDC (production)

Authorization Code flow with PKCE via the `openidconnect` crate, using
discovery against the issuer.

```toml
[auth]
mode = "oidc"

[auth.oidc]
issuer_url    = "https://idp.example/realms/main"
client_id     = "pkdb"
client_secret = "..."            # supplied via environment, not this file
redirect_url  = "https://pkdb.example/auth/callback"
scopes        = ["openid", "profile", "email"]
allowed_subs  = ["sub-abc123"]   # or allowed_emails
```

An unauthenticated request to `/ui/*` redirects to `/auth/login`, which
redirects to the IdP. `/auth/callback` verifies the `state` parameter, the PKCE
verifier, and the ID token's signature, `iss`, `aud`, `exp`, and `nonce`, then
checks the subject against the allowlist, creates a session row, and sets the
cookie.

**The allowlist is mandatory.** An empty allowlist denies every subject rather
than defaulting to open access. Without it, every account in the identity
provider can read the knowledge base.

Session cookies hold an opaque 256-bit random identifier and are set
`HttpOnly`, `Secure`, `SameSite=Lax`, with a 30-day sliding expiry backed by a
row in `sessions`. Sessions are server-side rather than a JWT in a cookie so
that revocation actually takes effect. `/auth/logout` deletes the row.

### Local (development only)

`auth.mode = "local"` uses a username and an argon2id password hash from
config. It logs a warning at startup and **refuses to bind to a non-loopback
address** unless `--i-know-this-is-insecure` is passed. That interlock is what
prevents a development shortcut from silently becoming production auth.

### API tokens

MCP clients and the future CLI cannot run a browser flow. After signing in
through the web UI, the operator mints a token at `/ui/tokens`.

- Format `pkdb_<43 characters base64url>`, from 32 bytes of CSPRNG entropy.
- Stored as an argon2id hash. Displayed once and never retrievable afterwards.
- Named, revocable, and tracked with `last_used_at`.
- Presented as `Authorization: Bearer pkdb_...`.

Both paths converge on one axum extractor that produces an `Identity`. `core`
never sees cookies or tokens.

Full OAuth 2.1 resource-server flow for MCP is deliberately deferred. `/mcp` is
written as a standard resource server, so OAuth can be placed in front of it
later without changing tool code.

## 9. Interfaces

### REST

| Path | Auth | Purpose |
|---|---|---|
| `POST /api/v1/sources` | session or token | Ingest; returns immediately |
| `GET /api/v1/sources/:id` | session or token | Raw text, status, chunks |
| `DELETE /api/v1/sources/:id` | session or token | Cascades to chunks and Qdrant points |
| `POST /api/v1/sources/:id/reprocess` | session or token | Re-run segment or embed |
| `GET /api/v1/search` | session or token | The hot path |
| `POST /api/v1/ask` | session or token | Opt-in generation |
| `GET /api/v1/chunks/:id` + `PATCH` + `DELETE` | session or token | Manual fix-ups; `PATCH` re-embeds |
| `GET /api/v1/status` | session or token | Queue depth, failed jobs, counts |
| `/mcp` | token | `ingest`, `search`, `ask` |
| `/auth/login`, `/auth/callback`, `/auth/logout` | none | Sign-in flow |
| `/healthz` | none | Liveness only, exposes no data |

### Web UI

Five screens:

- **Capture** — textarea plus optional title. Posts and clears instantly, then
  shows a processing indicator.
- **Search** — input with `hx-trigger="keyup changed delay:250ms"`. Results are
  rendered markdown cards with title, category, tags, and score.
- **Browse** — sources with status; drill into chunks.
- **Detail** — raw text beside rendered chunks, with inline edit and a
  reprocess button.
- **Ops** — queue depth, failed jobs with `last_error`, retry button, token
  management.

Editing a chunk's text re-enqueues its embed job on save, so vectors never go
stale.

## 10. Visual system

The palette and geometry are ported from
[Vestigo](https://github.com/overcuriousity/Vestigo)
(`frontend/src/index.css` and `frontend/src/components/ui/`).

### Tokens

Copied verbatim as CSS custom properties, switched by
`[data-theme="light"|"dark"]`, with `color-scheme` set per theme.

Light — warm paper: `--color-bg-base #f8f6f1`, surface `#f2f0ea`, elevated
`#ffffff`, hover `#ece9e2`, active `#e2ded3`. Foreground `#2d2d2d` /
`#5a5a5a` / `#7a7a72` / `#b8b5ac`. Borders `#ddd8cc`, strong `#c9c3b4`, subtle
`#eae7de`. Accent muted blue `#3b6e91` with `-dim` and `-muted` rgba variants
and `--color-accent-fg #ffffff`.

Dark — cool near-black: base `#0e1015`, surface `#14171d`, elevated `#1b1e26`,
overlay `#22252f`, hover `#262a34`, active `#2d3140`. Foreground `#e2e4ec` /
`#8b8fa8` / `#7d82a0` / `#3a3d50`. Borders `#232636`, strong `#2e3244`, subtle
`#191c26`. Accent muted teal `#5aa8b0` with `--color-accent-fg #0e1015`.

Status colors — danger, warning, success, info — each carry a `-dim` rgba wash
used as badge backgrounds. Shadows are defined at three sizes and tuned
separately per theme.

### Geometry

| Property | Value |
|---|---|
| Radii | 3px / 6px / 10px; 3px on buttons, inputs, and badges |
| Control heights | sm 32px, md 36px, lg 40px; icon 32x32 |
| Control padding | sm 12px, md 14px, lg 16px horizontal |
| Borders | 1px, `--color-border-strong` on interactive elements |
| Type scale | 16px root, 14px body, 12px meta and badges |
| Fonts | Inter 400/500/600; JetBrains Mono 400/500 |
| Transitions | 120ms ease on background, border-color, color, opacity |
| Focus ring | `1.5px solid var(--color-accent)`, `outline-offset: 2px` |
| Scrollbars | 6px, thumb `--color-border-strong`, 3px radius |
| Badges | mono, 12px, `padding: 2px 6px`, `-dim` background, colored text, 30% border |

### Deviations from Vestigo

1. **No Tailwind.** The UI is server-rendered with no node toolchain, and pkdb
   has roughly ten component types against Vestigo's twenty-four. Instead,
   one hand-written `app.css` holds the token blocks as plain CSS followed by
   semantic classes — `.btn`, `.btn-accent`, `.input`, `.badge`, `.card` —
   whose declarations are the literal values from Vestigo's `cva` variants.
   Rendering is identical and no build step is required. Tailwind can be
   adopted later on top of unchanged tokens if the UI outgrows this.
2. **Visualization palette dropped.** The eight-slot categorical series, the
   sequential and diverging ramps, and the chart chrome tokens serve Vestigo's
   charts. pkdb has none. The block can be restored if the Ops screen ever
   grows a graph.
3. **No density toggle.** `data-density="compact"` pays off in dense forensic
   grids, not in a reading-oriented search UI.

### Fonts

Self-hosted. Without node there is no `@fontsource` package, and a Google Fonts
CDN would mean an external request from the knowledge base plus a privacy leak.
Inter and JetBrains Mono `woff2` files are vendored into `assets/fonts/`,
embedded with `rust-embed`, and declared with `@font-face` using
`font-display: swap`. Roughly 200KB in the binary.

### Mapping to pkdb screens

Search results and chunk previews are `--color-bg-elevated` cards with
`--color-border` at `--radius-md`. Category and tag chips are badges in the
`accent` variant. Source status uses the status colors: `ready` maps to
success, `partial` to warning, `failed` to danger, and in-flight states to
accent. The Ops queue table follows Vestigo's table conventions.

## 11. Error handling

A `thiserror` enum in `core`, mapped once to HTTP in `web`:

| Variant | HTTP | Worker retries |
|---|---|---|
| `NotFound` | 404 | — |
| `Duplicate { existing_id }` | 200 with the existing source | — |
| `Validation` | 400 | no |
| `Unauthorized` / `Forbidden` | 401 / 403 | — |
| `Inference { role, source }` | 502 | yes |
| `Vector` | 502 | yes |
| `Store` | 500 | yes, limited |
| malformed LLM output | not surfaced | once, then fallback |

Only retryable variants consume job attempts. Classification happens at the
error site rather than by string-matching later, because a validation error
retried five times is five wasted inference calls.

**Ingest never fails on inference.** If the chunker is unreachable, ingest
still returns 201 and the source waits at `raw` with a backing-off job.

### Startup checks

These fail fast and loudly:

- SQLite migrations are applied.
- Qdrant is reachable and the collection exists. If the collection exists with
  a vector dimension different from `embed.dim`, **the process refuses to
  start** and reports both numbers. Silently writing 768-dimension vectors into
  a 1024-dimension collection surfaces weeks later as unexplained bad search
  results.
- Each inference endpoint gets one probe call. Failures are logged as warnings
  and are not fatal.

## 12. Configuration

`config.toml` with `PKDB__`-prefixed environment overrides, loaded through the
`config` crate. Secrets — the OIDC client secret and inference API keys — come
from the environment only; the loader warns if it finds a secret in the file.
`--print-config` prints the effective configuration with secrets redacted.

## 13. Observability

`tracing` with `tracing-subscriber`, filtered by `RUST_LOG`, with optional JSON
output. Each ingest opens a span carrying `source_id` that follows the document
through segmentation and embedding, so one grep reconstructs a document's full
history. Per-call timings are recorded for each inference role, which is what
distinguishes embedding latency from Qdrant latency.

`GET /api/v1/status` returns source counts by status, job counts by state,
failed jobs with `last_error`, and the age of the oldest pending job. The Ops
screen renders this.

## 14. Testing

Layered so that nearly everything runs under `cargo test` without containers.

1. **Unit** — token budgeting arithmetic, the structural pre-splitter, chunk
   JSON parsing including malformed input, markdown sanitization (asserting
   that `<script>` and `onerror=` are stripped), token hashing and
   verification, and the retry backoff schedule.
2. **Store** — real SQLite through `sqlx::test` with a fresh temporary database
   per test and migrations applied. Covers `content_hash` deduplication,
   cascade deletion, concurrent job claiming (two workers must never claim the
   same row), and FTS trigger behavior.
3. **Inference fakes** — `Chunker`, `Embedder`, and `Reranker` are traits, so
   tests inject deterministic fakes; the fake embedder hashes text into a
   fixed-dimension vector. Separate `wiremock` tests exercise the real HTTP
   surface: OpenAI-compatible wire format, timeouts, 429 responses, and
   malformed JSON.
4. **Vector fake** — an in-memory `VectorStore` implementation doing brute-force
   cosine similarity, which allows the full ingest-to-search path to be tested
   with no infrastructure.
5. **Integration** — marked `#[ignore]` and opt-in, running against a real
   Qdrant from docker-compose to exercise filtered search and payload indexes.
   CI runs these; a local `cargo test` skips them.
6. **HTTP** — axum `oneshot` against the router. Every route is asserted to
   return 401 when unauthenticated, since a missing auth check is the failure
   mode that matters most here. Also covers deduplication returning the
   existing id and search filter passthrough.

Retrieval *quality* is not tested. That requires a labeled evaluation set that
does not exist yet. A `pkdb eval` subcommand running a YAML file of query and
expected-chunk pairs is noted as post-v1 work and is not built now.

## 15. Build order

1. Skeleton: configuration, tracing, SQLite with migrations, `/healthz`.
2. Ingest and the sources API, using fakes, with no inference.
3. Job runner and state machine.
4. Real `infer` module: segmentation, then embedding.
5. Qdrant and search.
6. Auth: OIDC, local mode, and API tokens.
7. Web UI on the ported visual tokens.
8. MCP server.
9. `ask`, reprocess, and the Ops screen.

Each step ends with something runnable. The first real search works at step 5.

## 16. Post-v1, explicitly out of scope

- Hybrid search combining FTS5 and vectors (the schema supports it).
- Rerank enabled by default (configuration supports it).
- CLI.
- `pkdb eval` and retrieval quality measurement.
- OAuth 2.1 resource-server flow for `/mcp`.
- File upload and non-text ingestion.
