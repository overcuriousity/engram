<img src="assets/wordmark.svg" alt="engram" width="236">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste text, a link, a PDF or
a photo; engram splits it into passages, embeds them, and answers queries with
ranked excerpts — your words, not a rewrite of your words. That distinction is
the whole product, and most tools get it wrong. Three front doors over one
backend: a web UI, a REST API at `/api/v1`, and an MCP server at `/mcp`, so an
agent can read and write the base mid-session. Three doors. Nobody has ever
needed a fourth.

**Your text, not a summary of it.** Search returns the source wording, and every
artifact stays anchored to the lines of the corpus it came from, so a result can
always be read beside the original. Asking a question *across* artifacts is a
separate endpoint you call deliberately. Nothing generated stands between you
and your own notes unless you ask for it.

**Rewriting is earned.** Capture cannot know which of ten thousand paragraphs
will ever be asked about, so engram spends no model call on them up front. A
window is rewritten into a self-contained artifact once reading has shown it is
worth rewriting — a passage opened often enough, or a run of searches that
assembled an answer the base did not hold. Every synthesized artifact can name
the use that earned it. The rest of your text stays exactly as you wrote it.
That is `infer.synthesis = "earned"`, the default; `"eager"` rewrites everything
at capture, `"off"` rewrites nothing and needs no chat model at all.

**It measures its own retrieval.** A test query written while looking at an
artifact reuses that artifact's wording, and every retrieval system on earth
passes it. Meaningless. The only honest question is one asked in earnest, before
anything came back — so engram records real searches and lets you judge them
later. The number on the judge page is not a proxy score. It is recall@10 and
MRR, read from the positions those searches actually gave.

## What it does

- **Capture** — paste text, fetch a URL, upload a PDF, drop or photograph an
  image, send from the browser extension, share from a phone, or pipe from a
  shell. Originals are kept untouched and served back. PDFs are read locally
  with no model; images need `[infer.vision]`.
- **One door for anything** — `POST /api/v1/capture` reads what it is handed
  rather than asking the caller to classify it: a body that is one link is a
  link, a PDF or an image arrives as raw bytes, and a multipart share of four
  photos is four captures.
- **On a phone** — installed on Android, engram joins the system share sheet.
  On iOS, a bookmarklet and a Shortcut recipe do the same, each carrying a token
  minted for that one device and revocable on its own.
- **From a shell** — the same binary is a client of a running engram. Capture,
  search, ask and read, drawn on a terminal and plain text in a pipe. See
  [The client](#the-client).
- **Search by meaning, and by the exact string** — a dense embedding and a
  locally computed BM25 vector are fused per query, because meaning finds the
  paragraph you half-remember and characters find `E01`, `--dry-run` and
  `/usr/bin/env`, which embeddings blur. Loose matches are labelled loose, and
  a divider marks where relevance falls off: the hits below it keep their rank
  but stop claiming to be answers.
- **Ask** — one question across the base, streamed, with planned retrieval for
  questions spanning several subjects. Answers abstain out loud when the base
  has nothing, and any command, path or flag the model wrote that appears in no
  cited excerpt is badged as unsupported. Tremendously useful, that badge.
- **Judging** — recorded searches and questions come back shuffled and
  unlabelled, one keystroke each. Everything stays on the machine and one button
  forgets all of it.
- **Duplicate hygiene** — near-duplicates are parked at capture, close pairs go
  to a review queue, and consolidation supersedes or merges only where it is
  safe. Nothing is deleted, no merge drops a number, command or path, and every
  action has an undo.
- **Associative memory** — links learned from co-retrieval and a decaying
  accessibility per artifact, so what you actually use stays reachable. Learned
  from use; never rewrites what is stored.
- **Knowledge gaps** — questions answered with nothing and searches judged as
  gaps are grouped, named, and listed until you cover them.

Everything after the paste runs on its own, and sweeps repair whatever was
interrupted. You do not babysit it.

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
