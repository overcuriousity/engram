<img src="assets/wordmark.svg" alt="engram" width="260">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Capture text, and engram
turns it into self-contained markdown artifacts, embeds them, and answers
queries with ranked excerpts — no generation step in the way. Answering a
question across several artifacts is a separate endpoint you ask for
explicitly.

Three front doors over one backend: a web UI, a REST API, and an MCP server, so
Claude Code or Claude Desktop can read and write it mid-session.

## Corpus, segment, artifact

Three words, and everything else follows from them.

A **corpus** is what you paste: a chapter, a manual, a transcript. It is stored
verbatim and never edited, and it is the provenance every answer can be traced
back to.

A **segment** is a slice of one corpus, sized to fit the model's context. The
split is free, local and mechanical — it prefers a heading, then a blank line,
then cuts where the budget runs out. A segment stores line numbers rather than
a copy of the text, and doubles as the memory that lets a failed run resume
instead of restarting.

An **artifact** is what the model makes from a segment: a passage rewritten to
stand on its own, with a title, a category and tags. Artifacts are what gets
embedded, ranked, read and edited. When you search, artifacts are what comes
back.

Each artifact records the segment it came from and the corpus lines it claims,
which is what lets the detail pane show a rewritten passage beside the original
wording it was made from. One model call turns one segment into several
artifacts; no artifact spans two segments.

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
through `raw → synthesizing → embedding → ready` on Browse. `partial` means some
artifacts failed to embed; the Ops screen lists the failures with a retry button.

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
| `vector.pinned_boost` | Extra score for an artifact tagged `pinned`. Default 0.15. |
| `infer.synthesize.*` | Synthesis model: `base_url`, `model`, `context_tokens`, `max_output_tokens`, `output_ratio`, optional `tokenizer_path`, `timeout_secs`, `reasoning_effort`, `cooldown_secs`. |
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
- **`infer.synthesize.output_ratio`** — synthesis rewrites artifacts to be
  self-contained, so its output can be larger than its input. It is also what
  sizes the input segment: `min(context / (1 + ratio), max_output / ratio)`.
  Raise it if synthesis responses get truncated; the default of 8.0 gives
  ~2000 tokens of input per call, which a 9B model can rewrite without running
  out of room.
- **`infer.embed.max_input_tokens`** must be the *server's* ceiling, not the
  model's nominal one. llama.cpp refuses any input above its physical batch
  size — often 1024 — with a 500 that no retry can fix. engram splits a refused
  artifact rather than retrying it, but it splits sooner and cheaper when this
  number is honest.
- **`timeout_secs`** defaults to 900. That is absurd for a hosted API and about
  right for a local model — see below.

## Running against a small local model

This is the case engram is built for, and it behaves differently enough from a
hosted API to be worth stating.

A segment against a 9B model on one consumer GPU has been measured
at seven minutes and 8000 output tokens. Three consequences shape the defaults:

- **Timeouts must outlast the model.** A timeout is indistinguishable from a
  dead endpoint to the job runner: the call fails, the job retries, and it
  fails again at the same wall. Hence 900 seconds by default, and per role so a
  fast embedder is not held to synthesis's patience.
- **Reasoning tokens come out of the output budget.** A reasoning model thinks
  before it writes any JSON, and that spending is taken from the same
  `max_output_tokens` the artifact list has to fit in. Set
  `reasoning_effort = "none"` if your endpoint honours it, and keep
  `max_output_tokens` generous. The field is unset by default because models
  that do not reason reject it.
- **A truncated answer is normal, not exceptional.** When the artifact list is cut
  off mid-object, the artifacts that did finish are kept rather than the segment
  being retried — asking a slow model to do it again is the most expensive
  thing engram can do.

Two knobs exist for the hardware rather than the output. `cooldown_secs` idles
between segments so a long corpus is not one unbroken thermal load — it saves no
energy, since the same tokens are generated either way, but it lets the card
settle. And `workers = 1` is right when every role points at one GPU: a second
worker does not double throughput, because the inference server serialises the
calls regardless.

What does save energy is generating fewer tokens: `reasoning_effort = "none"`
where the endpoint honours it, and not re-synthesising work that already
succeeded — which is what the per-segment resume is for.

Synthesis logs each call's duration and token count, because minutes of
silence otherwise looks exactly like a hang. Browse shows `synthesising 3/9`
for the same reason.

## Inference roles

`synthesize`, `embed` and `ask` are configured separately and can point at different
servers. Reranking is opt-in because there is no OpenAI-standard rerank endpoint,
so its wire format is configured rather than guessed.

Ingest never calls inference. Capture writes the text and returns; synthesis
and embedding run in the background with retries. A dead endpoint slows
processing but never loses a capture.

Synthesis runs one segment at a time and remembers where it got to. A segment
that succeeds is written before the next is attempted, so a retry resumes
rather than re-paying for the segments that already worked, and a segment the
chunker cannot handle is split structurally on its own lines while the rest
keep their their artifacts. That corpus is reported `partial`, and Ops names
the segment. Browse shows `synthesising 3/9` while it happens.

Paste a chapter at a time. A book works — it is windowed the same way — but it
costs one model call per segment, and a chapter is what search results read best
from. The capture screen says so and warns above roughly one segment's worth of
text. The request body limit is an explicit 8 MB.

## Does the artifact still say what the corpus said?

Each segment is checked before its artifacts are stored.

- **Literals.** Commands, paths and flags in an artifact must appear in the segment
  it came from, compared with whitespace normalised. If they do not, the segment
  is synthesised once more; failing that, the artifact is stored with a flag naming
  the literal that went missing. A paraphrased command is a command that later
  gets pasted into a root shell, and losing the chapter to protect against that
  would be worse than a warning the reader can see.
- **Spans.** An artifact's claimed `corpus_lines` are clamped to its own segment and
  checked for plausible overlap with the lines they name. The detail pane
  renders those lines beside the artifact, so a wrong span is not cosmetic. Models
  omit `corpus_lines` more often than not; when that happens the span is
  recovered by finding the artifact's own verbatim lines in the segment, and only a
  span the model actually asserted is ever doubted.
- **Coverage.** The fraction of a corpus's lines that ended up inside some
  artifact is recorded and shown on Browse. Below 60% it reads as a warning — a
  corpus where synthesis dropped half a chapter used to look identical to
  one where it did not.

Flagged artifacts are listed on Ops with two actions: re-synthesise that one segment,
or mark the artifact reviewed.

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

**One corpus leads with at most three artifacts**, so a forty-artifact document
cannot take the top of the list from the rest of the corpus. What that displaces
goes back on the end in rank order, so the cap reorders a result list rather
than shortening it — a base holding a single document still answers with as much
as it has. `ask` opts out of the reordering entirely: an answer often lives in
one document, and it reads better in rank order.

**Recency and pinning** are applied as a final scoring pass. The weights let
recency break a near-tie but never overturn a clearly better match. Pinning is a
tag: `PATCH /api/v1/artifacts/{id}` with `{"tags": ["pinned"]}`.

**`GET /api/v1/resurface`** returns a random handful of artifacts older than a month
that have not appeared in results since. Every search records what it showed, so
surfacing something counts as remembering it.

Result **scores are ranking scores, not similarities**. A hybrid query returns a
fused rank, a query with no indexable term returns a cosine, and both then carry
the recency term. They order one result list and mean nothing between two, which
is why the UI shows a position rather than a number.

The search page keeps the ranked list beside the result. Opening a hit fills a
detail pane with the artifact and the corpus lines it claims, so a paraphrase is
visible without leaving the page; `/ui/artifacts/{id}` is the same view as a
standalone page, for links and new tabs. Query terms are highlighted, long
artifacts clamp with an expand control, and every fenced block has a copy button.

Typing is cheap: query embeddings are cached, so a burst of keystrokes costs one
embedding call rather than one per prefix. Incremental searches do not record
what they showed — only opening an artifact, or a deliberate API, MCP or `ask` call,
does. That is what keeps `resurface` meaningful.

Editing an artifact's `tags` or `category` rewrites the Qdrant payload in place.
Editing `text` or `title` queues a re-embed — those are what the model was shown.
An absent field in a `PATCH` is left alone; an explicit `null` clears it.

An edit that lands while an artifact is being embedded wins: artifacts carry a revision
that vector-invalidating edits bump, and the in-flight job's "indexed" mark only
applies while the revision still matches. A losing job leaves the artifact pending,
which is what gets it embedded again from the text that is actually there.

## Is the ranking any good?

The settings above — fusion, the per-corpus cap, recency weight, reranking on or
off, which embedding model — are guesses until something measures them. Typing a
few searches does not: you phrase the query with words you remember from the
passage you want, so it comes back first and everything looks fine. The case
that actually fails is the one where you half-remember something and describe it
in words the document never uses.

The evaluation harness answers that with two numbers over pairs you write once:
**recall@10**, whether the right artifact was on the page at all, and **MRR**, how
far down it was.

The corpus is whatever documents you actually want to search, and it stays on
your machine — nothing about it is in this repository:

```
$ENGRAM_EVAL_DIR/corpus/*.txt   your documents
$ENGRAM_EVAL_DIR/artifacts.json    written by eval-prepare
$ENGRAM_EVAL_DIR/pairs.json     written by hand
```

Freeze the artifacts once, so a benchmark run costs no completions and two runs
rank exactly the same text:

```bash
ENGRAM_EVAL_DIR=~/engram-eval cargo run --bin eval-prepare
```

Then write pairs: a query phrased the way you would really type it, and the id
of the artifact that should answer it. Pairs that reuse the artifact's own vocabulary
measure nothing, because every retrieval system passes them. The useful ones
share almost no words with their answer.

```json
[
  { "query": "handy war aus als die polizei kam",
    "expect": "01J8ZK…",
    "note": "BFU vs AFU" }
]
```

Running it needs a live Qdrant and embedding endpoint, so it is ignored by
default:

```bash
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
```

```
20 queries over 143 artifacts   (embed bge-m3, rerank off, recency 0.05, cap 3)
recall@10   0.75   (15/20)
MRR         0.52

missed:
  handy war aus als die polizei kam                  not returned
  wie finde ich raus wann die datei geschrieben      rank 8
```

Settings come from configuration, so comparing two of anything is a loop rather
than a rebuild:

```bash
ENGRAM__VECTOR__RECENCY_WEIGHT=0.0 ENGRAM_EVAL_CAP=none \
  ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
```

Re-running `eval-prepare` mints new artifact ids, so the pairs have to be
re-checked afterwards. The harness refuses to score a pair whose expected artifact
no longer exists rather than counting it as a ranking failure forever.

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
Qdrant will not allow both. Freeing the name means deleting the corpus after its
points are copied and counted, so that case needs `--reindex --replace-legacy`.

A rebuild cannot change vector width. See below for that.

## Changing the embedding model

1. Update `infer.embed.model` and `infer.embed.dim`.
2. Point `vector.collection` at a new alias name, or delete the existing
   generations.
3. Start engram; it creates a fresh generation at the new dimension.
4. Re-embed: `POST /api/v1/corpora/{id}/reprocess` with `{"stage":"embed"}`.

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

- Artifact text is model output rendered into an authenticated session, so it is
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
- Artifact metadata is bounded on the way in: an artifact carries at most 32 tags of
  64 characters, because tags become payload on every point and a keyword index
  in Qdrant.
- CI runs `cargo audit` on every push. Advisories are ignored one id at a time
  in `.cargo/audit.toml`, each with a written reachability argument — currently
  one entry, RUSTSEC-2023-0071 in `rsa` via `openidconnect`, which concerns
  private-key operations that engram never performs.

## Backup

SQLite is the corpus of truth; it holds every raw capture verbatim.

- **Full:** copy `engram.db` (plus `-wal`/`-shm`) and the Qdrant volume.
- **Minimal:** copy `engram.db` alone. Vectors can be regenerated by
  reprocessing, at the cost of re-running embeddings.

## Development

```bash
cargo test                                         # no containers needed
cargo test --test integration_qdrant -- --ignored  # needs a running Qdrant
cargo test --test eval -- --ignored --nocapture    # needs Qdrant, an embedder
cargo fmt --check                                  #   and a corpus; see above
cargo clippy --all-targets -- -D warnings
```

Set `ENGRAM_TEST_QDRANT` if Qdrant is not on `localhost:6333`. Everything except
the Qdrant suite runs without infrastructure: inference and the vector store sit
behind traits, and the tests inject fakes plus an in-memory vector store.
