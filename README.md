<img src="assets/wordmark.svg" alt="engram" width="260">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste text, a link, a PDF or
a photo; engram splits it into passages, embeds them, and answers queries with
ranked excerpts — your words, not a rewrite of them. Three front doors over one
backend: a web UI, a REST API at `/api/v1`, and an MCP server at `/mcp`, so an
agent can read and write the base mid-session.

## Why it is built this way

**Your text, not a summary of it.** Search returns the source wording, and every
artifact stays anchored to the lines of the corpus it came from, so a result can
always be read beside the original. Asking a question *across* artifacts is a
separate endpoint you call deliberately — there is no generation step standing
between you and your own notes.

**Rewriting is earned.** Capture cannot know which of ten thousand paragraphs
will ever be asked about, so engram spends no model call on them up front. A
window is rewritten into a self-contained artifact once reading has shown it is
worth rewriting — a passage opened and confirmed often enough, or a run of
searches that assembled an answer the base did not hold. Every synthesized
artifact can name the use that earned it; the rest of your text stays exactly as
you wrote it. That is `infer.synthesis = "earned"`, the default. `"eager"`
rewrites everything at capture; `"off"` never rewrites anything and needs no
chat model at all.

**It measures its own retrieval.** A test query written while looking at an
artifact reuses that artifact's wording, and every retrieval system passes it.
The only uncontaminated question is one asked in earnest, before anything came
back — so engram records real searches and lets you judge them later. The
counter on the judge page is not a proxy score: it is recall@10 and MRR, read
from the positions those searches actually gave.

## Features

- **Capture** — paste text, fetch a URL, upload a PDF, drop or photograph an
  image, or send from the browser extension. Originals are kept untouched and
  served back. PDFs are read locally with no model; images need `[infer.vision]`.
- **Search by meaning** — type the situation, not the keywords. Loose matches
  are labelled as loose, and where the scores fall off a cliff the hits past it
  are greyed: they keep their rank but stop claiming to be answers.
- **Ask** — one question across the base, streamed over SSE, with planned
  retrieval for questions spanning several subjects. Answers abstain out loud
  when the base has nothing, and any command, path or flag the model wrote that
  appears in no cited excerpt is badged as unsupported.
- **Judging and evaluation** — recorded searches and questions come back
  shuffled and unlabelled, one keystroke each; `--export-eval` writes the judged
  pairs out for the offline harness. Everything stays on the machine and one
  button forgets all of it.
- **Duplicate hygiene** — near-duplicates are parked at capture, close pairs go
  to a review queue, and consolidation supersedes or merges only where it is
  safe. Nothing is deleted, no merge drops a number, command or path, and every
  action has an undo.
- **Associative memory** — links learned from co-retrieval and a decaying
  accessibility per artifact, so what you actually use stays reachable. Learned
  from use; never rewrites what is stored.
- **Knowledge gaps** — questions answered with nothing and searches judged as
  gaps are grouped, named, and listed until you cover them.
- Everything after the paste runs on its own, and sweeps repair whatever was
  interrupted.

## Corpus, segment, artifact

A **corpus** is what you captured — a chapter, a manual, a transcript, a photo,
a PDF. Stored verbatim, never edited, and the provenance every answer traces
back to.

A **segment** is a slice of one corpus, sized to fit the model's context. The
split is local and mechanical, and it doubles as the memory that lets an
interrupted run resume instead of restarting.

An **artifact** is a unit of retrieval: text with a title, category and tags,
embedded and ranked on its own. Most are *passages* — verbatim slices, split on
the document's own headings. A *synthesized* or *captured* artifact is one the
model wrote from a window, badged as such wherever it is shown, and retired with
one click. No artifact spans two segments.

## Requirements

- Rust 1.94+ (the floor comes from sqlx 0.9).
- Qdrant, reachable over its REST API. No gRPC port needed.
- An OpenAI-compatible endpoint for chat completions and embeddings. One server
  can fill every role, or each role can point somewhere different.

## Deployment

```bash
cargo build --release        # target/release/engram
docker compose up -d         # Qdrant on 127.0.0.1:6333
cp config.example.toml config.toml
./target/release/engram --hash-password 'your password'   # paste into config.toml
./target/release/engram
```

Qdrant's [released binary](https://github.com/qdrant/qdrant/releases) works just
as well as the container; engram only needs its REST port.

Open <http://127.0.0.1:8080/auth/login>, capture something, and watch it move
through `raw → embedding → ready` on Browse. `partial` means part of it has not
come through yet; Ops says what is retrying and when, and nothing there needs
you.

`--config` takes an explicit path; otherwise `config.toml` in the working
directory is read if present, and a configuration supplied entirely through the
environment needs no file at all. `--print-config` shows what engram resolved,
with secrets redacted.

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

Other one-shot commands: `--reindex` copies every vector into a fresh collection
generation and swaps the alias onto it, `--export-eval DIR` writes the
evaluation pairs, `--recompute-coverage` re-measures corpus coverage from stored
artifacts. Each of them exits when done, and each takes `--user <SUBJECT>`
naming the base it acts on — see below.

## Multiple users

Every user gets their own SQLite database and their own Qdrant collection. No
user data is shared, and there is no query anywhere that could be written
without a tenant filter, because no tenant filter exists: the isolation is
structural rather than a predicate. Every setting in `config.toml` stays
instance-wide.

What is *not* divided is compute. There is one embed endpoint, one synthesize
endpoint, one reranker, and most likely one GPU behind all of them, so
`server.workers` stays one number however many people sign up: it is the
admission point in front of that hardware. A worker pool per user would let ten
signed-in users fire `10 × server.workers` concurrent requests at a single
endpoint, where throughput does not scale but collapses. Adding a user costs a
file and a collection; it does not cost a thread pool.

Set `auth.mode = "oidc"`. The first request from an unseen subject provisions
that user — a row, a database, a collection. There is no registration UI and no
password management: the identity provider owns accounts, and engram owns
nothing but the mapping. `auth.mode = "local"` stays a development shortcut, and
provisions one tenant keyed on the configured username.

Three keys under `[store]`, all optional:

| Key | Default | |
|---|---|---|
| `control_path` | `engram-control.db` | Identity, sessions, tokens, and the one job queue |
| `dir` | `data/users` | Where the per-user databases live, one `{slug}.db` each |
| `max_open_tenants` | `32` | How many bases may be open at once; eviction is transparent |

### Accounts

```bash
engram --list-users                      # subject, slug, email, judge grant
engram --grant-judge  sub-abc123
engram --revoke-judge sub-abc123
engram --delete-user  sub-abc123         # row, credentials, file and alias, behind a typed yes
```

The judge grant gates the whole of `/ui/judge`, which is also the only route in
the tree that writes `config.toml` — applying a tuning recommendation moves the
instance's ranking parameters, so it is not something every account should
reach. There is no admin role; the flag is granted out of band, per user, and
takes effect on the next request rather than on a restart — the Judge entry in
the nav appears on the same request the route opens on, because both read the
column rather than the copy of the row the registry is holding. The raw form
works too:

```bash
sqlite3 engram-control.db "UPDATE users SET can_judge = 1 WHERE subject = '…'"
```

`--reindex`, `--export-eval` and `--recompute-coverage` each require
`--user <SUBJECT>`. Omitting it is an error listing the known subjects rather
than a default: defaulting to an arbitrary tenant is how the wrong collection
gets reindexed.

### Adopting an existing single-user base

Set `migrate.adopt_subject` to the OIDC subject of whoever has been using it and
start engram. It moves `store.path` to `{store.dir}/{slug}.db`, writes the user
row with the judge granted, and renames the existing Qdrant alias onto
`{vector.collection}_{slug}`. The alias moves rather than the collections behind
it, so nothing re-embeds and the generation history `--reindex` walks is
preserved.

Three tables do not travel with the file, because they moved to the control
database: `api_tokens`, `sessions` and `jobs`. Adoption copies them across under
the adopting subject, so existing API tokens keep working — the browser
extension's included — the browser you had open stays signed in, and work that
was queued when the old process stopped resumes. Expired sessions and finished
jobs are not carried; a claimed job comes across as pending, since the process
that was holding it is the one that stopped. The originals stay in the moved
file, unread, in case you want to look at them.

It is guarded on the `users` table being empty, so it fires exactly once and
cannot go off on a running instance however the file is edited afterwards. Every
step that can fail is either before the user row is written or rolled back with
it — the file move, the alias rename, the copy above — because a half-adopted
install that boots reads as a base whose searches have gone empty, and a user
row left behind is one that turns adoption off for good.

### Backup

A backup is the control database **plus** every file under `store.dir`, taken
together. They reference each other: the queue names subjects, the subjects name
files. Restoring one side from a different moment than the other shows up as
store drift — an artifact one store holds and the other does not — which
`heal_store_drift` repairs per tenant on that tenant's next open, and which the
Ops page reports in the meantime.

## Configuration

Any key can be set by environment variable: prefix `ENGRAM__`, `__` between
levels, e.g. `ENGRAM__INFER__EMBED__DIM=768`. Put secrets there rather than in
the file — the loader warns if it finds one.

### Inference endpoints

engram speaks one protocol, the OpenAI-compatible HTTP API, and does not care
who serves it. A local server on your own GPU — llama.cpp, vLLM, or anything
else exposing `/v1` — and a hosted OpenAI-compatible API are the same
configuration with a different `base_url`. Mixing them is normal: bulk work on
local hardware, the interactive answer on a hosted model, or the reverse.

A chat endpoint is named once under `[infer.tiers.<name>]` and pointed at by
role, so moving synthesis or `ask` to another model is one word. Embedding is
configured separately under `[infer.embed]`, because an embedding endpoint is a
different shape of thing rather than a cheaper model.

```toml
# A local server. No key, and a timeout measured in minutes.
[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 16384

# A hosted OpenAI-compatible API. Key from the environment, and a far shorter
# timeout — 900 seconds is right for a local model, absurd for a hosted one.
[infer.tiers.deep]
base_url = "https://api.example.com/v1"
model = "some-large-model"
context_tokens = 128000
max_output_tokens = 8192
timeout_secs = 120
# api_key via ENGRAM__INFER__TIERS__DEEP__API_KEY

[infer.synthesize]
tier = "efficient"

[infer.ask]
tier = "deep"
plan_tier = "efficient"
```

Hosted endpoints are also where `ceiling_param` and `reasoning_effort` matter:
reasoning models generally refuse `max_tokens` and want `max_completion_tokens`,
and they spend part of the output ceiling thinking before they write anything.
Left unset, engram guesses and corrects itself from the endpoint's own 400.

### Embedding

`[infer.embed]` is configured on its own, with no `tier`: an embedding endpoint
is a different shape of thing, not a cheaper chat model. Local or hosted makes
no difference here either — `base_url` plus an `api_key` from the environment.

The defaults are EmbeddingGemma's: 768 dimensions, a 2048-token window, and an
*asymmetric* interface, meaning queries and documents are embedded through
different prompts. Those three prompts are the model card's exact strings, so
nothing below `max_input_tokens` needs setting for it:

```toml
[infer.embed]
base_url = "http://localhost:8000/v1"
model = "embeddinggemma"
dim = 768
max_input_tokens = 2048     # the *server's* ceiling, not the model's nominal one
timeout_secs = 120
# query_template             = "task: search result | query: {text}"
# document_template          = "title: {title} | text: {text}"
# document_template_untitled = "title: none | text: {text}"
```

A *symmetric* embedder wants the text as it is — that is three lines and a
different width:

```toml
model = "bge-m3"
dim = 1024
max_input_tokens = 1024
query_template             = "{text}"
document_template          = "{title}\n{text}"
document_template_untitled = "{text}"
```

`model`, `dim` and the three templates together are one identity: a vector
embedded under one recipe is not comparable with one embedded under another, and
nothing in the vector says which it was. engram fingerprints all five and says
so at boot when the stored vectors no longer match the configured recipe. There
is no rebuild path — the answer is to drop the collection and re-capture.

Check once that your serving stack does not prepend a prompt of its own, or the
prefix is sent twice and every vector carries it.

### Reranking

Optional and off by default. `[infer.rerank]` re-scores the candidates search
already fetched; it can only narrow what it is given, never reach for something
the vector query missed, and a rerank failure degrades ordering rather than
availability — engram logs it and returns vector order.

There is no OpenAI-standard rerank endpoint, so the wire format is configured
rather than guessed. `style` picks it:

| `style` | Request |
|---|---|
| `tei` | `POST {base_url}/rerank` with `{query, texts}` |
| `cohere` | `POST {base_url}/rerank` with `{model, query, documents, top_n}` |
| `vllm` | `POST {base_url}/v1/rerank` |

```toml
[infer.rerank]
base_url = "http://localhost:8001"
model = "bge-reranker-v2-m3"
style = "tei"
# timeout_secs = 900
# api_key via ENGRAM__INFER__RERANK__API_KEY
```

### Keys

| Key | Meaning |
|---|---|
| `server.bind` | Listen address. Default `127.0.0.1:8080`. |
| `server.workers` | Background job workers. One is right for a single local GPU. |
| `store.path` | SQLite file. **Back this up.** |
| `vector.url` / `vector.collection` / `vector.api_key` | Qdrant REST URL, and the alias the data lives behind as `{name}_v1`, `_v2`, … |
| `vector.recency_weight` / `recency_half_life_days` / `pinned_boost` / `weak_below` | Ranking: how much age counts against a hit (0.05 / 180 days), the boost for a `pinned` tag (0.15), and the cosine under which a hit is labelled loose (0.35). |
| `infer.tiers.<name>.*` | A named chat endpoint: `base_url`, `model`, `api_key`, `context_tokens`, `max_output_tokens`, `timeout_secs`, `reasoning_effort`, `ceiling_param`, `structured_output`. |
| `infer.synthesis` | `"earned"` (default), `"off"` or `"eager"`. At `off` both chat roles may be omitted entirely. |
| `infer.synthesize.*` | `tier`, `output_ratio`, `context_opening_tokens`, `context_overlap_tokens`. Also carries the dedupe judge, link judge, gap namer and claim check. |
| `infer.ask.*` | `tier`, `plan`, `plan_tier`. Any tier field may be overridden here. |
| `infer.embed.*` | `base_url`, `model`, `api_key`, `dim`, `max_input_tokens`, `timeout_secs`, `chunk_tokens`, and the three prompt templates. See [Embedding](#embedding). |
| `infer.rerank.*` | Optional, off by default: `base_url`, `model`, `api_key`, `style`, `timeout_secs`. See [Reranking](#reranking). |
| `infer.vision.*` | Optional, off by default — it is what opens the image door. `model` is required; `base_url` and `api_key` fall back to the synthesize role's. |
| `consolidate.*` | Duplicate hygiene thresholds and the rate the dedupe judge runs at. |
| `feedback.*` | Recording real searches for later judging. On by default; `retain_days` applies to unjudged searches only. |
| `associate.*` / `activation.*` / `promote.*` | Learned links, decaying accessibility, and the activation at which a passage earns synthesis. |
| `pursuit.*` | Off by default. A run of searches that engaged several artifacts without the base answering earns one generated artifact. |
| `pacing.cooldown_secs` | Minimum gap between background inference calls. `ask` ignores it. |
| `auth.mode` / `auth.oidc.*` / `auth.local.*` | `oidc` for anything reachable; `local` is development only and loopback only. |

### Worth knowing before you start

- **`infer.embed.dim` must match the collection.** If it does not, engram
  refuses to start and names both numbers. Mismatched vectors corrupt search in
  a way you would not notice for weeks — see [Embedding](#embedding) for the
  rest of that identity.
- **`infer.embed.max_input_tokens` is the server's ceiling, not the model's.**
  llama.cpp refuses input above its physical batch size — often 1024 — with a
  500 that no retry can fix.
- **`infer.synthesize.output_ratio`** sizes the input window as
  `min(context / (1 + ratio), max_output / ratio)`. Raise it if responses get
  truncated; 8.0 gives ~2000 tokens of input per call.
- **`infer.ask.max_output_tokens`** comes out of `context_tokens`: `ask`
  reserves it and packs excerpts into the remainder, so raising it buys longer
  answers by showing the model fewer of them. Never more than half the window.
- **Asking needs JavaScript** — the answer streams over SSE.
  `POST /api/v1/ask` and the MCP `ask` tool are the JS-free ways in.

`config.example.toml` carries every key with the reasoning behind each default.
