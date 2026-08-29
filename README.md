<img src="assets/wordmark.svg" alt="engram" width="236">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste anything — text, a
link, a PDF, a photo. engram splits it, embeds it, and hands back the passages
that answer you. Your words. Ranked. Never a rewrite.

Everybody else summarizes your notes and shows you the summary. Then the summary
is all you have. We keep the original and we keep it in front of you.

Three doors, one backend: the web UI, a REST API at `/api/v1`, and an MCP server
at `/mcp` so an agent can read and write mid-session. Three doors is enough.

**Nothing gets rewritten until it earns it.** Capture spends no model call on
paragraphs nobody will ever ask about, and that is most of them. A passage is
rewritten once you have actually used it, and it says so. That is
`infer.synthesis = "earned"`, the default. `"eager"` rewrites everything up
front; `"off"` rewrites nothing and needs no chat model at all.

**Both halves of retrieval.** A dense embedding for meaning, a local BM25 vector
for characters. Meaning finds the paragraph you half-remember. Characters find
`E01` and `--dry-run`, which embeddings blur. You get both, fused, every query.

**It grades itself, honestly.** A test query written while looking at the answer
passes on every system ever built. Meaningless. engram records the searches you
made in earnest and lets you judge them later — recall@10 and MRR, from the
positions those searches really gave. Not a proxy score.

## What it does

- **Capture anything** — paste, a URL, a PDF, a photo, the browser extension,
  the phone share sheet, a shell pipe. One endpoint reads what it is handed
  instead of asking you to classify it. Originals are stored untouched and
  served back. PDFs are read locally with no model; images need
  `[infer.vision]`.
- **Search** — loose matches say they are loose, and a divider marks where
  relevance falls off. Below the line, hits keep their rank and stop pretending.
- **Ask** — one question across the whole base, streamed. It abstains out loud
  when the base has nothing. Any command or path the model wrote that no excerpt
  supports gets badged. That badge is the best part.
- **From a shell** — capture, search, ask, read. Drawn on a terminal, plain text
  in a pipe. See [The client](#the-client).
- **Judge** — a result you read, or answer *Was this what you were looking
  for?* under, is a labelled pair; what that leaves comes back as a card, top
  five in order, one keystroke each. It all stays on your machine, and one
  button forgets it.
- **Duplicates** — near-duplicates parked at capture, close pairs queued for a
  person. Nothing deleted. No merge drops a number, a command or a path. Undo on
  everything.
- **Memory that learns** — links from co-retrieval, accessibility that decays,
  so what you use stays reachable. It never rewrites what is stored.
- **Gaps** — questions the base could not answer are grouped, named and listed
  until you cover them.

Everything after the paste runs on its own, and sweeps repair whatever was
interrupted. You do not babysit it.

## Corpus, segment, artifact

A **corpus** is what you captured. Stored verbatim, never edited, and the
provenance every answer traces back to.

A **segment** is a slice of one corpus, sized to the model's context. Local and
mechanical, and it doubles as the memory that lets an interrupted run resume.

An **artifact** is a unit of retrieval: text with a title, category and tags,
ranked on its own. Most are verbatim *passages*, split on the document's own
headings. A *synthesized* or *captured* one was written by the model from a
window, badged wherever it is shown, and retired with one click.

## Three rules

Everything on the issue tracker is weighed against these.

**Inference happens at write time, not read time.** A search costs one
embedding, one vector search and a few indexed SQLite reads — never a
generation. Making retrieval better means making the background job do more,
never adding a model call to the query path. *Ask* is the one door that
generates at read time; every call it spends is bounded and visible on the page.

**The trace is fixed; access is plastic.** Content is verbatim and never changes
silently: a captured artifact is never rewritten in place, and nothing is
deleted on a score. Consolidation is the one narrow exception and carries its
own guards — originals kept and undoable, no value or literal lost. Everything
about *how* an artifact is found may learn from use, within bounds that are
shown: a primed hit says so, an associated hit says what recalled it, and no
exact match is ever buried.

**Lean beats clever.** Anything that adds a storage tier, a model dependency or
a layer crossing without a measured retrieval gain does not go in. The
evaluation harness is the only figure comparable across months; a default that
changes ranking moves only after it has been run.

Decided against, and not coming back: generated answer cards or answers stored
as artifacts without the operator asking (a digest competing with the wording it
was derived from); model-written "situations" as extra vectors (a guess deciding
what surfaces); a resurfacing list of things you had forgotten (a different
application); LLM excerpt compression at query time (the cliff and the reranker
do it for free); late-interaction reranking (a model dependency to beat a
baseline hybrid search already makes strong); quantization to save memory
nobody has run out of.

## Requirements

- Rust 1.94+ (the floor comes from sqlx 0.9).
- Qdrant, reachable over its REST API. No gRPC port needed.
- An OpenAI-compatible endpoint for chat completions and embeddings. One server
  can fill every role, or each role can point somewhere different.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/overcuriousity/engram/master/install.sh | sh
```

It takes the build for the machine it runs on — a statically linked `x86_64`
one that needs no libc of any particular vintage, or `aarch64` — checks it
against the release's `SHA256SUMS`, and installs to `/usr/local/bin` where that
is writable and `~/.local/bin` otherwise. It never reaches for sudo.
`ENGRAM_INSTALL_DIR` says where to put it instead, `ENGRAM_VERSION` pins a tag.
The script is [`install.sh`](install.sh) here and is worth reading before you
pipe it to a shell; the archives are on the [releases
page](https://github.com/overcuriousity/engram/releases) if you would rather
take them by hand. Versions are dates — `v2026.827.0` — and are marked
pre-release while the shape of things is still moving.

Or build it: `cargo build --release`, giving `target/release/engram`.

## First run

Qdrant has to be answering on the address in `config.toml` — `127.0.0.1:6333`
by default — before engram starts. How you run it is your business: the
[released binary](https://github.com/qdrant/qdrant/releases) and a container are
equally fine, and only its REST port is ever spoken to.

```bash
cp config.example.toml config.toml
engram --hash-password 'your password'   # paste into config.toml
engram
```

Open <http://127.0.0.1:8080/auth/login>, capture something, and watch it move
through `raw → embedding → ready` on Browse. `partial` means part of it has not
come through yet; Ops says what is retrying and when, and nothing there needs
you.

`--config` takes an explicit path; otherwise `config.toml` in the working
directory is read if present, and a configuration supplied entirely through the
environment needs no file at all. `--print-config` shows what engram resolved,
with secrets redacted.

### As a service

With the binary at `/usr/local/bin/engram` and the config and database under
`/var/lib/engram`:

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

### One-shot commands

| | |
|---|---|
| `--reindex` | Copies every vector into a fresh collection generation and swaps the alias onto it |
| `--export-eval DIR` | Writes the judged evaluation pairs out for the offline harness |
| `--recompute-coverage` | Re-measures corpus coverage from stored artifacts |

Each exits when done, and each requires `--user <SUBJECT>` naming the base it
acts on. Omitting it is an error listing the known subjects rather than a
default: defaulting to an arbitrary tenant is how the wrong collection gets
reindexed.

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
endpoint, where throughput does not scale but collapses.

Set `auth.mode = "oidc"`. The first request from an unseen subject provisions
that user — a row, a database, a collection. There is no registration UI and no
password management: the identity provider owns accounts, and engram owns
nothing but the mapping. Who may sign in is still engram's to say, and it has to
be said: list the people the instance is for in `allowed_subs`,
`allowed_emails` or `allowed_groups`, and a subject matching any one entry is
admitted. A configuration naming nobody is refused at startup rather than read
as naming everybody — because provisioning is what admission costs, and against
a provider that allows self-registration, an open instance is a stranger
creating a database and a vector collection here, with nothing capping how many
times. A deployment that genuinely wants the provider to be the only gate says
so with `open_registration = true`.

### Accounts

```bash
engram --list-users                      # subject, slug, email, judge grant
engram --grant-judge  sub-abc123
engram --revoke-judge sub-abc123
engram --delete-user  sub-abc123         # row, credentials, file and alias, behind a typed yes
```

`--delete-user` drops the Qdrant collections first and refuses to go any further
if it cannot reach them: the alias name is derived from the subject, so a
collection left behind is not merely orphaned — the next time that person signs
in, the surviving alias is adopted and the deleted account comes back with every
vector it had. Nothing is deleted when it stops that way, so the fix is to bring
Qdrant back and run it again.

The judge grant gates the whole of `/ui/judge`, which is also the only route in
the tree that writes `config.toml` — applying a tuning recommendation moves the
instance's ranking parameters. There is no admin role; the flag is granted out
of band, per user, and takes effect on the next request rather than on a
restart. The raw form works too:

```bash
sqlite3 engram-control.db "UPDATE users SET can_judge = 1 WHERE subject = '…'"
```

### Backup

A backup is the control database **plus** every file under `store.dir`, taken
together. They reference each other: the queue names subjects, the subjects name
files. Restoring one side from a different moment than the other shows up as
store drift — an artifact one store holds and the other does not — which
`heal_store_drift` repairs per tenant on that tenant's next open, and which the
Ops page reports in the meantime.

## Configuration

`config.example.toml` carries every key with the reasoning behind each default.
What follows is what it cannot tell you from inside itself.

Any key can be set by environment variable: prefix `ENGRAM__`, `__` between
levels, e.g. `ENGRAM__INFER__EMBED__DIM=768`. Put secrets there rather than in
the file — the loader warns if it finds one.

### One dial over the learning layer

`learn.mode` is the line to set first, and on a base that only wants capture,
search and ask it is the only one from that half of the file:

```toml
[learn]
mode = "full"     # "off" | "learning" | "full"
```

- `off` — record nothing, learn nothing, prime nothing, promote nothing;
  consolidation is left with the exact and near duplicates capture finds for a
  hash. A config naming `[server]`, `[vector]`, `[infer.embed]`, `[auth]` and
  this one line starts and searches.
- `learning` — searches and asks are recorded, activation and links are
  written, and nothing reads any of it on the query path: no priming, no
  associative spread, no promotion, no offers under the search box, and no
  pursuit generation — the corpus holds still as well as the ranking. This is
  the mode to run `cargo test --test eval` in. A default that changes ranking
  moves only after it has been measured, and it cannot be measured while its
  own inputs are moving the ranking it is measured against, or growing the
  corpus it is measured over.
- `full` — the defaults, unchanged.

Every key the mode stands for is still a key, and one written in the file wins
over what the mode would have said. `--print-config` names the mode first and
then the keys it decided, so which of the two you are looking at is never a
guess.

### Inference endpoints

engram speaks one protocol, the OpenAI-compatible HTTP API, and does not care
who serves it. A local server on your own GPU — llama.cpp, vLLM, anything
exposing `/v1` — and a hosted API are the same configuration with a different
`base_url`. Mixing them is normal: bulk work on local hardware, the interactive
answer on a hosted model, or the reverse.

A chat endpoint is named once under `[infer.tiers.<name>]` and pointed at by
role, so moving synthesis or `ask` to another model is one word:

```toml
[infer.tiers.efficient]
base_url = "http://localhost:8000/v1"
model = "qwen"
context_tokens = 32768
max_output_tokens = 16384

[infer.tiers.deep]
base_url = "https://api.example.com/v1"
model = "some-large-model"
context_tokens = 128000
max_output_tokens = 8192
timeout_secs = 120     # 900 is right for a local model, absurd for a hosted one
# api_key via ENGRAM__INFER__TIERS__DEEP__API_KEY

[infer.synthesize]
tier = "efficient"

[infer.ask]
tier = "deep"
plan_tier = "efficient"
```

Reasoning models generally refuse `max_tokens` and want `max_completion_tokens`,
and they spend part of the output ceiling thinking before they write anything —
that is what `ceiling_param` and `reasoning_effort` are for. Left unset, engram
guesses and corrects itself from the endpoint's own 400.

### Embedding

`[infer.embed]` is configured on its own, with no `tier`: an embedding endpoint
is a different shape of thing, not a cheaper chat model. The defaults are
EmbeddingGemma's — 768 dimensions, a 2048-token window, and an *asymmetric*
interface where queries and documents are embedded through different prompts, in
the model card's exact strings. A *symmetric* embedder wants the text as it is,
which is three lines and a different width:

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

`max_input_tokens` is the *server's* ceiling, not the model's nominal one:
llama.cpp refuses input above its physical batch size, often 1024, with a 500
that no retry can fix. And check once that your serving stack does not prepend a
prompt of its own, or the prefix is sent twice and every vector carries it.

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

### Worth knowing before you start

- **`infer.embed.dim` must match the collection.** If it does not, engram
  refuses to start and names both numbers. Mismatched vectors corrupt search in
  a way you would not notice for weeks.
- **`infer.ask.max_output_tokens` comes out of `context_tokens`.** `ask`
  reserves it and packs excerpts into the remainder, so raising it buys longer
  answers by showing the model fewer of them. Never more than half the window.
- **Asking needs JavaScript** — the answer streams over SSE. `POST /api/v1/ask`
  and the MCP `ask` tool are the JS-free ways in.

## The client

Everything above is the server's `config.toml`. The client half of the same
binary never reads it: it talks to a running engram over HTTP and needs only an
address and a token, which it reads from `~/.config/engram/cli.toml`. On a
laptop that is only ever a client, the [installer](#install) is the whole of the
install, and `ENGRAM_INSTALL_DIR=~/.local/bin` keeps it out of a directory
needing root.

```bash
engram -c notes.pdf                 # capture; `pbpaste | engram` captures a pipe
engram -s 40 "loop device"          # search, as wide as you ask
engram -a "how did I mount it?"     # stream an answer
engram --show 3                     # read the third hit of the last search in full
```

`--show` also takes a leading piece of an id or a whole one, and the sources
under an `-a` answer are numbered as the same kind of list, so the `[9]` an
answer cites is `engram --show 9`. Exit `1` means nothing was found, so
`engram -s "x" || …` is a usable branch.

```toml
url = "http://127.0.0.1:8080"     # or ENGRAM_URL
token = "engram_…"                # minted under /ui/settings; or ENGRAM_TOKEN
```

Run the client before that file exists and it writes it for you, commented and
`0600`, then says to put a token in it — an existing config is never touched.
`cli.example.toml` in the release archive is the same text.
