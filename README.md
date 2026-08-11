<img src="assets/wordmark.svg" alt="engram" width="260">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste text and engram
rewrites it into self-contained markdown artifacts, embeds them, and answers
queries with ranked excerpts — no generation step in the way. Asking a question
across several artifacts is a separate endpoint you call explicitly.

Everything after the paste happens on its own: splitting, synthesis, embedding,
duplicate hygiene, and the sweeps that repair whatever was interrupted. Every
artifact stays anchored to the lines of the source it came from, so a rewritten
passage can always be read beside the original wording.

Three front doors over one backend: a web UI, a REST API, and an MCP server, so
Claude Code or Claude Desktop can read and write it mid-session.

## Corpus, segment, artifact

A **corpus** is what you paste: a chapter, a manual, a transcript. Stored
verbatim, never edited, and the provenance every answer traces back to.

A **segment** is a slice of one corpus, sized to fit the model's context. The
split is local and mechanical — a heading, then a blank line, then wherever the
budget runs out. It stores line numbers rather than a copy, and doubles as the
memory that lets a failed run resume instead of restarting.

An **artifact** is what the model makes from a segment: a passage rewritten to
stand on its own, with a title, a category and tags. Artifacts are what gets
embedded, ranked, read and edited. One call turns one segment into several
artifacts; no artifact spans two segments.

## Asking for something

Type the situation, not the keywords. The query is embedded whole and matched
against artifacts written to stand alone, so a sentence — or the paragraph you
happen to be staring at — carries far more signal than the two nouns you would
distil it into. "the customer says the file was never on the stick but I see an
entry with no start cluster" is a better query than "FAT deleted entry", and it
is a query you can paste rather than compose.

Keywords still work; they are simply the weakest thing you can hand it.

## Learning what the search got wrong

Whether that ranking is any good is a question nothing in the app can answer on
its own, and it cannot be answered from memory either: a test query written
while looking at an artifact reuses the artifact's wording, and every retrieval
system passes it. The only uncontaminated question is one you asked in earnest,
before you saw what came back.

So with `feedback.enabled`, every search is recorded — including the one where
you found nothing and gave up, which leaves no other trace and is the most
telling of all. Later, at `/ui/judge`, each recorded search comes back as a card
with its candidates shuffled and unlabelled, and one question: which of these
was the one you needed? Four answers, all one keystroke: a number, `N` for none
of these (then find what should have answered), `S` to skip, `X` if it was not a
real search.

The counter at the top is not a score standing in for the measurement — it *is*
recall@10 and MRR, read from the positions those searches actually gave.

```bash
engram --export-eval ~/engram-eval          # artifacts.json + pairs.json
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval -- --ignored --nocapture
```

The export reads SQLite only: no inference, no Qdrant, and the artifacts keep
their real ids, so running it again does not invalidate the pairs. Nothing
leaves the machine, `enabled` is off until you turn it on, and Ops has a button
that forgets all of it.

## Requirements

- Rust 1.94+ (the floor comes from sqlx 0.9).
- Qdrant, reachable over its REST API — 6333 locally, or whatever a proxy
  serves it on. No gRPC port needed.
- An OpenAI-compatible endpoint for chat completions and embeddings. One server
  can fill every role, or each role can point somewhere different.

## Setup

Build the binary:

```bash
cargo build --release        # target/release/engram
```

Run Qdrant alongside it — the [released binary](https://github.com/qdrant/qdrant/releases)
is enough; engram only needs its REST port.

Write a config and a password hash:

```bash
cp config.example.toml config.toml
./target/release/engram --hash-password 'your password'   # paste into config.toml
./target/release/engram
```

Open <http://127.0.0.1:8080/auth/login>, capture something, and watch it move
through `raw → synthesizing → embedding → ready` on Browse. `partial` means part
of it has not come through yet; Ops says what is retrying and when, and nothing
there needs you.

`--config` takes an explicit path; otherwise `config.toml` in the working
directory is read if present, and a configuration supplied entirely through the
environment needs no file at all. `--print-config` prints what engram actually
resolved, with secrets redacted.

As a service, with the binary at `/usr/local/bin/engram` and the config and
database under `/var/lib/engram`:

```ini
[Unit]
Description=engram
After=network-online.target

[Service]
User=engram
WorkingDirectory=/var/lib/engram
ExecStart=/usr/local/bin/engram --config /var/lib/engram/config.toml
Environment=ENGRAM__AUTH__OIDC__CLIENT_SECRET=…
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`auth.mode = "local"` refuses to bind to anything but loopback. Anything
reachable from another machine wants `oidc`, or a reverse proxy that
authenticates in front of it.

## Configuration

Any key can be set by environment variable: prefix `ENGRAM__`, `__` between
levels, e.g. `ENGRAM__INFER__EMBED__DIM=768`. Put secrets there rather than in
the file — the loader warns if it finds one.

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
| `vector.weak_below` | Cosine under which a result is labelled "loose" rather than presented as an answer. Default 0.35; `0.0` turns it off. |
| `infer.synthesize.*` | Synthesis model: `base_url`, `model`, `context_tokens`, `max_output_tokens`, `output_ratio`, optional `tokenizer_path`, `timeout_secs`, `reasoning_effort`, `cooldown_secs`. |
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`, `timeout_secs`. |
| `infer.ask.*` | Completion model, used only by `ask`. Same timeout and reasoning keys. |
| `infer.rerank.*` | Optional. `style` is `tei`, `cohere` or `vllm`. Off by default. |
| `consolidate.*` | Duplicate hygiene: `enabled`, `near_dupe_min`, `review_min`, `auto_supersede`, `sample`, `per_point`, `interval_hours`, `judge`, `max_judgements`. |
| `feedback.*` | Recording real searches for later judging: `enabled`, `candidates`, `coalesce_secs`, `retain_days`. Off by default. |
| `auth.mode` | `oidc` or `local`. |
| `auth.oidc.*` | `issuer_url`, `client_id`, `client_secret`, `redirect_url`, `scopes`, `allowed_subs` / `allowed_emails` / `allowed_groups`. |
| `auth.local.*` | `username` and an argon2id `password_hash`. Development only. |

Four worth knowing:

- **`infer.embed.dim`** must match the collection. If it does not, engram
  refuses to start and names both numbers. Mismatched vectors corrupt search in
  a way you would not notice for weeks.
- **`infer.synthesize.output_ratio`** — synthesis rewrites artifacts to stand
  alone, so its output can exceed its input. It also sizes the input segment:
  `min(context / (1 + ratio), max_output / ratio)`. Raise it if responses get
  truncated; 8.0 gives ~2000 tokens of input per call.
- **`infer.embed.max_input_tokens`** must be the *server's* ceiling, not the
  model's nominal one. llama.cpp refuses input above its physical batch size —
  often 1024 — with a 500 no retry can fix.
- **`timeout_secs`** defaults to 900. Absurd for a hosted API, about right for a
  local model answering in minutes.

`config.example.toml` carries the same keys with the reasoning behind each
default.
