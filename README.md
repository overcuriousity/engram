<img src="assets/wordmark.svg" alt="engram" width="236">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste anything — text, a
link, a PDF, a photo. engram stores it verbatim, splits and embeds it, and hands
back the passages that answer you, rewritten for retrieval only where that helps.

Everybody else summarizes your notes and shows you the summary. Then the summary
is all you have. We keep the original and we keep it in front of you.

Three doors, one backend: the web UI, a REST API at `/api/v1`, and an MCP server
at `/mcp` so an agent can read and write mid-session. Three doors is enough.

**Both halves of retrieval.** A dense embedding for meaning, a local BM25 vector
for characters. Meaning finds the paragraph you half-remember; characters find
`E01` and `--dry-run`, which embeddings blur. Both, fused, every query.

**It grades itself, honestly.** A test query written while looking at the answer
passes on every system ever built. engram instead reads recall@10 and MRR off
the verdicts you give under real searches, at the positions those searches
really gave. Not a proxy score.

## What it does

- **Capture anything** — paste, a URL, a PDF, a photo, the browser extension,
  the phone share sheet, a shell pipe. One endpoint reads what it is handed
  instead of asking you to classify it; originals are stored untouched. PDFs are
  read locally with no model; images need `[infer.vision]`. Small pastes are
  structured on the spot, big ones stored verbatim and rewritten once you have
  used them, and the box says which fate a paste will meet before you press it.
- **Search** — loose matches say they are loose, and a divider marks where
  relevance falls off; below it, hits keep their rank and stop pretending. Hold
  the microphone and talk, where `[infer.transcribe]` names a speech endpoint.
- **Ask** — one question across the base, streamed. It abstains out loud when
  the base has nothing, and badges any command or path the model wrote that no
  excerpt supports. That badge is the best part.
- **Judge** — a result you read, or answer *Was this what you were looking for?*
  under, is a labelled pair. Insights reads recall@10 and MRR off those
  verdicts, and the idle pass replays them, beside what use left behind, to move
  the ranking on its own. It stays on your machine; one button forgets it.
- **Duplicates** — near-duplicates parked at capture, close pairs queued for a
  person. Nothing deleted, no merge drops a number or a path, undo on everything.
- **Memory that learns** — links from co-retrieval and accessibility that
  decays, so what you use stays reachable. While you are away it sleeps: files
  the day's captures against what it held, rehearses that against your later
  wording, takes back what the evidence says it got wrong, and says so on
  Insights. Everything it writes is a version beside the original, never over it.
- **Gaps** — questions the base could not answer, grouped and named until you
  cover them.
- **Reap** — what has been retired for 90 days gets one more look from a model:
  whatever it still states that the live base does not is rewritten live, and
  the rest leaves search and index for a graveyard table nothing reads back. Off
  with `reap.enabled = false`, and never touching what an open reminder names.
- **Time** — dates are read by the same synthesis call that structures a
  capture, so a reminder is judged rather than pattern-matched. What is due
  shows under the box with done, snooze and push to Gotify or UnifiedPush, and a
  finished reminder retires its note: still searchable, no longer one of the
  last things you kept. Every *today* links to the day page.

Everything after the paste runs on its own, and sweeps repair whatever was
interrupted. You do not babysit it.

## Corpus, segment, artifact

A **corpus** is what you captured: verbatim, never edited, the provenance every
answer traces back to. A **segment** is a slice of one sized to the model's
context — local, mechanical, and the memory that lets an interrupted run resume.

An **artifact** is a unit of retrieval: text with a title, category and tags,
ranked on its own and sized to embed whole. Most are verbatim *passages*, split
on the document's own structure. A *synthesized* or *captured* one was written
by the model, badged wherever it is shown, and supersedes the passages it covers
without deleting them.

## What a capture becomes

One endpoint, one box, one `-c`. There is no "new reminder" form and no kind to
pick: the call that structures a capture also reads what it is.

| It becomes | When the text | Where it shows |
| --- | --- | --- |
| a **note** | states something | search, Ask, the corpus view |
| a **reminder** | asks to be reminded | the due band, then a push |
| a **journal entry** | recounts your day | the day page |
| an **event** | mentions a date without asking for anything | *Coming up*, the day page |
| a **link** | restates or answers a note you kept | the rail beside both |

A note is the floor: everything captured is one, and the other four are things a
note additionally *is*. Nothing leaves search by becoming one of them.

The reader is a model, not a rule table, so this is guidance and not a syntax.
It keys on the sentence's *stance* — what you are asking of the note.

```text
remind me Friday to send the invoice      → reminder, Friday 09:00
every Monday, back up the NAS             → reminder, repeating
the workshop is on the 14th               → event: a date, nothing owed
long day, but it built                    → journal entry
PUID is Microsoft's per-user identifier   → note
```

**A date is not a reminder.** *The invoice is dated 30 September* mentions one;
*pay the invoice by 30 September* asks for something. The first is an event
under *Coming up*, only the second wakes your phone. An unstated time of day is
09:00, and repetition survives as a recurring row. Reminders show for
`time.horizon_hours` before they are due (48) and events for
`time.coming_up_days` (7).

`engram -r` and `?intent=remind` are a hint on the prompt, not a filing: the
model still reads the sentence, and the hint buys a dateless reminder still
being armed for the band to ask about. `engram -j` is a filing, taken at the
door. If a filing is wrong, say so on the row — *not a reminder* on the band,
the toggle on the day page — and the refusal survives a re-read.

## Three rules

- Inference happens at write time, not read time.
- Nothing is lost, and every step is readable and reversible.
- Lean beats clever.

Everything on the issue tracker is weighed against these.

## Requirements

Rust 1.94+ (the floor comes from sqlx 0.9), Qdrant over its REST API (no gRPC
port needed), and an OpenAI-compatible endpoint for chat and embeddings. One
server can fill every role, or each role can point somewhere different.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/overcuriousity/engram/master/install.sh | sh
```

It takes the build for the machine it runs on, checks it against the release's
`SHA256SUMS`, and never reaches for sudo. `ENGRAM_INSTALL_DIR` says where to put
it, `ENGRAM_VERSION` pins a tag. Read [`install.sh`](install.sh) before piping it
to a shell; the archives are on the [releases
page](https://github.com/overcuriousity/engram/releases) if you would rather take
them by hand. Versions are dates — `v2026.918.1` — marked pre-release while the
shape of things is still moving. Or build it: `cargo build --release`.

## First run

Qdrant must be answering on the address in `config.toml` — `127.0.0.1:6333` by
default — before engram starts. Only its REST port is ever spoken to.

```bash
cp config.example.toml config.toml
engram --hash-password 'your password'   # paste into config.toml
engram
```

Open <http://127.0.0.1:8080/auth/login>, capture something, and watch it move
through `raw → embedding → ready` on Browse. `partial` means part of it has not
arrived; Ops says what is retrying, and nothing there needs you.

`--config` takes an explicit path, otherwise `config.toml` in the working
directory is read if present — a configuration supplied entirely through the
environment needs no file. `--print-config` shows what engram resolved, redacted.

### As a service

Binary at `/usr/local/bin/engram`, config and database under `/var/lib/engram`:

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
reachable from another machine wants `oidc`, or a proxy authenticating in front.

### One-shot commands

`--reindex` copies every vector into a fresh collection generation and swaps the
alias onto it. `--recompute-coverage` re-measures corpus coverage from stored
artifacts. Each exits when done and requires `--user <SUBJECT>`; omitting it is
an error listing the known subjects, because defaulting to an arbitrary tenant
is how the wrong collection gets reindexed.

## Multiple users

Every user gets their own SQLite database and Qdrant collection. Nothing is
shared, and no query anywhere could be written without a tenant filter because
no tenant filter exists — the isolation is structural. Every setting in
`config.toml` stays instance-wide.

What is *not* divided is compute. One embed endpoint, one synthesize endpoint,
one reranker, most likely one GPU behind all of them, so `server.workers` stays
one number however many people sign up: it is the admission point in front of
that hardware. A pool per user would let ten signed-in users fire
`10 × server.workers` concurrent requests at one endpoint, where throughput does
not scale but collapses.

Set `auth.mode = "oidc"`. The first request from an unseen subject provisions
that user — a row, a database, a collection. There is no registration UI and no
password management: the provider owns accounts, engram owns the mapping. Who
may sign in is still engram's to say, in `allowed_subs`, `allowed_emails` or
`allowed_groups`; a subject matching any one entry is admitted. An allowlist
naming nobody is refused at startup rather than read as "everybody", because
provisioning is what admission costs: against a provider with open
self-registration, every stranger is a database and a vector collection created
here, uncapped. A deployment that wants the provider to be the only gate says so
with `open_registration = true`.

### Accounts

```bash
engram --list-users                      # subject, slug, email
engram --delete-user  sub-abc123         # row, credentials, file and alias, behind a typed yes
```

`--delete-user` drops the Qdrant collections first and stops if it cannot reach
them. The alias name is derived from the subject, so a collection left behind is
worse than orphaned: the next time that person signs in, the surviving alias is
adopted and the deleted account returns with every vector it had. Nothing is
deleted when it stops that way — bring Qdrant back and run it again.

There is no admin role, and nothing in the tree writes `config.toml`: the
ranking tunes itself under `[evolve]`, and the file is your starting point.

### Backup

A backup is the control database **plus** every file under `store.dir`, taken
together — the queue names subjects, the subjects name files. Restoring one side
from a different moment than the other shows up as store drift, which
`heal_store_drift` repairs per tenant on that tenant's next open and the Ops
page reports in the meantime.

## Clients

**The command line** is the other half of the same binary. It never reads the
server's `config.toml`: it talks to a running engram over HTTP and needs an
address and a token, from `~/.config/engram/cli.toml`. Run it before that file
exists and it writes it for you, commented and `0600`.

```bash
engram -c notes.pdf                 # capture; `pbpaste | engram` captures a pipe
engram -s 40 "loop device"          # search, as wide as you ask
engram -a "how did I mount it?"     # stream an answer
engram -r "call the bank tomorrow"  # a hint that this is a reminder
engram -j "long day, but it built"  # today's journal entry
engram --show 3                     # read the third hit of the last search in full
```

`--show` also takes a leading piece of an id. Sources under an `-a` answer are
numbered as the same kind of list, so the `[9]` an answer cites is
`engram --show 9`. Exit `1` means nothing was found, so `engram -s "x" || …` is
a usable branch.

**Android** is a client of `/api/v1` and nothing more. It pairs by scanning a QR
the server shows, over a two-minute window, pinning the certificate it is given;
set `server.tls_fingerprint` and it pins from the first byte. Search is home,
Ask beside it, the due band beneath. Capture goes through an outbox, so a
capture on a train is a capture. The microphone is the web's — held rather than
pressed, drawn only where the server reports a speech model. Building it and
what each screen does: [android/README.md](android/README.md). The APK is on the
[releases page](https://github.com/overcuriousity/engram/releases).

**The browser extension** captures the page you are on, for Chrome and Firefox —
[extension/README.md](extension/README.md).

## Configuration

[`config.example.toml`](config.example.toml) carries every key with the reasoning
behind each default, including embedding recipes, reranker wire formats, and the
`[infer.tiers.<name>]` blocks that let each role point at its own endpoint. Any
key can be set by environment variable: prefix `ENGRAM__`, `__` between levels,
e.g. `ENGRAM__INFER__EMBED__DIM=768`. Put secrets there rather than in the file;
the loader warns if it finds one.

What that file cannot tell you from inside itself:

**`learn.mode` is the line to set first** — on a base that only wants capture,
search and ask, the only one from that half of the file.

```toml
[learn]
mode = "full"     # "off" | "learning" | "full"
```

`off` records nothing and learns nothing; a config naming `[server]`,
`[vector]`, `[infer.embed]`, `[auth]` and this one line starts and searches.
`learning` records searches and writes links and activation, but reads none of
it on the query path — the mode to gather evidence in before anything may move a
rank, since a change cannot be measured while its own inputs are moving the
ranking it is measured against. `full` is the defaults. Every key a mode stands
for is still a key, and one written in the file wins; `--print-config` names the
mode first and then the keys it decided.

**Three things worth knowing before you start:**

- **`infer.embed.dim` must match the collection.** If it does not, engram
  refuses to start and names both numbers. Mismatched vectors corrupt search in
  a way you would not notice for weeks.
- **`infer.ask.max_output_tokens` comes out of `context_tokens`.** `ask`
  reserves it and packs excerpts into the remainder, so raising it buys longer
  answers by showing the model fewer of them. Never more than half the window.
- **Asking needs JavaScript** — the answer streams over SSE. `POST /api/v1/ask`
  and the MCP `ask` tool are the JS-free ways in.

## Docs

- [docs/api.md](docs/api.md) — what a client of `/api/v1` may rely on.
- [docs/evaluation.md](docs/evaluation.md) — how the base measures its own
  retrieval, and what the idle pass does with the result.
- [android/README.md](android/README.md) — building the app, screen by screen.
- [extension/README.md](extension/README.md) — the browser extension.
