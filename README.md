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

A corpus can also be a **photo or image** — a whiteboard, a receipt, a page,
a diagram — dropped on the capture page, taken with the phone camera through
the installed PWA, pasted from the clipboard, or sent to
`POST /api/v1/corpora/image` (multipart `image`, optional `note` and
`title_hint`). Needs `[infer.vision]` in the config; without it the image door
is closed. The original file is kept untouched and served at
`GET /api/v1/corpora/{id}/image?original=1`; a vision model reads it into
markdown in the background, and from there it is a text corpus like any other.
File facts and EXIF (time, camera, location) are recorded on the corpus and
handed to the model as context. Every file door also accepts a short `note` —
your own words about what the file is — which is stored with the corpus and,
for an image, read by the model too. A photo the model cannot read — the
endpoint refuses it, it answers with nothing, or the retries run out — is shown
as `failed` with the reason on its page; **Re-read** on that page (or
`POST /api/v1/corpora/{id}/reprocess` with `{"stage":"describe"}`) reads it
again from the stored original, with whatever model is configured now.

A **segment** is a slice of one corpus, sized to fit the model's context. The
split is local and mechanical — a heading, then a blank line, then wherever the
budget runs out. It stores line numbers rather than a copy, and doubles as the
memory that lets a failed run resume instead of restarting.

An **artifact** is what the model makes from a segment: a passage rewritten to
stand on its own, with a title, a category and tags. Artifacts are what gets
embedded, ranked, read and edited. One call turns one segment into several
artifacts; no artifact spans two segments.

Most artifacts are *captured* — written from one segment of one corpus, with the
lines they came from shown beside them. A few are *merged*: written by
consolidation out of two or more captured artifacts that said the same thing,
and listing what they were written from instead of corpus lines. The originals
are hidden rather than deleted, one button restores them, and no merge is
written that would drop a number, command or path any of its sources carried.
Consolidation prefers to keep an original wherever one will do, so merging is
what happens when neither artifact alone was sufficient.

## Asking for something

Type the situation, not the keywords. The query is embedded whole and matched
against artifacts written to stand alone, so a sentence — or the paragraph you
happen to be staring at — carries far more signal than the two nouns you would
distil it into. "the customer says the file was never on the stick but I see an
entry with no start cluster" is a better query than "FAT deleted entry", and it
is a query you can paste rather than compose.

Keywords still work; they are simply the weakest thing you can hand it.

The list says what it knows about itself. A hit whose match is loose is
labelled so; when every hit is loose, the page says nothing matches closely.
And where the scores fall off — one step between two hits that is larger than
all the other steps put together — the rail draws a rule, and the hits past it
are greyed. They still placed and keep their rank; they have stopped claiming to
be answers. Over the API the same hit carries `past_cliff: true`, and over MCP
its meta line says "below the relevance cliff".

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
real search. Every candidate can be opened and read in full before you confirm
it, and `U` takes the last verdict back — a keystroke fast enough to judge with
is a keystroke fast enough to slip on, and a mislabelled pair is worse than none
at all.

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

Questions get the same treatment. With `feedback.enabled`, every question asked
on the ask page is recorded with the excerpts the model saw, and the answer
carries a verdict bar — right, wrong, nothing here — plus a "carried the
answer" toggle on each excerpt. An answer that opens with *Not in the knowledge
base* is an abstention and is badged as one. A literal in the answer — a
command, a path, a version — that appears in no cited excerpt is marked, and
badged: the model wrote it, and the base does not hold it.

The answer arrives over SSE as the model writes it, so **`/ui/ask` needs
JavaScript**. `POST /api/v1/ask` and the MCP `ask` tool do not, and are the
JS-free ways in; they answer with the whole thing at once, from the same code
path the page streams. `--export-eval` writes the judged
questions to `questions.json`, and

```bash
ENGRAM_EVAL_DIR=~/engram-eval cargo test --test eval evaluate_ask -- --ignored --nocapture
```

measures citation recall, abstention accuracy and faithfulness by literals;
`ENGRAM_EVAL_CLAIMS=1` adds a claim-by-claim check by the synthesize model.
Questions judged "nothing here" and searches judged `gap` are the base's holes:
they are grouped by meaning, named once by the synthesize model, and listed as
**Knowledge gaps** on the capture page until you mark them covered.

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
| `infer.tiers.<name>.*` | A named endpoint the chat roles point at: `base_url`, `model`, `api_key`, `context_tokens`, `max_output_tokens`, `timeout_secs`, `reasoning_effort`, `ceiling_param`, `structured_output`. Name them what you like; `efficient` and `deep` are the convention. |
| `infer.synthesize.*` | Synthesis: `tier`, plus `output_ratio`, `context_opening_tokens`, `context_overlap_tokens`, `cooldown_secs`. Any tier field may be overridden here. Also carries the dedupe judge, the link judge, the gap namer and the claim check. |
| `infer.embed.*` | Embedding model: `base_url`, `model`, `dim`, `max_input_tokens`, `timeout_secs`. No tier — an embedding endpoint is a different shape of thing, not a cheaper model. |
| `infer.ask.*` | Used only by `ask`: `tier`, `follow_up`, `follow_up_tier`. Any tier field may be overridden here. |
| `infer.rerank.*` | Optional. `style` is `tei`, `cohere` or `vllm`. Off by default. |
| `infer.vision.*` | Optional. Reads captured images: `model`, `base_url`, `api_key`, `timeout_secs`, `max_output_tokens`, `ceiling_param`. `base_url` and `api_key` default to the synthesize role's, and `ceiling_param` is inherited with them. Off by default. |
| `consolidate.*` | Duplicate hygiene: `enabled`, `near_dupe_min`, `review_min`, `auto_supersede`, `per_point`, `interval_hours`, `dedupe_interval_mins`, `max_dedupe_per_tick`, `merge_max_roots`. |
| `feedback.*` | Recording real searches for later judging: `enabled`, `candidates`, `coalesce_secs`, `retain_days` (unjudged searches only), `sweep_hours`. Off by default. |
| `auth.mode` | `oidc` or `local`. |
| `auth.oidc.*` | `issuer_url`, `client_id`, `client_secret`, `redirect_url`, `scopes`, `allowed_subs` / `allowed_emails` / `allowed_groups`. |
| `auth.local.*` | `username` and an argon2id `password_hash`. Development only. |

Eight worth knowing:

- **`infer.ask.follow_up`** lets the model say once what it still needs and
  retrieve a second time before answering. It costs one extra call per question
  and ships **off**: a default here moves after the harness has run, not before.
  `follow_up_tier` puts that call on a cheaper tier than the answer it feeds,
  which is the whole reason tiers are named.
- **A role may still carry its endpoint inline** instead of naming a tier.
  That shape parses and warns, naming the block to write instead; it is not an
  error, because making `tier` required would turn five keys you chose on
  purpose into unknown keys and ignore them silently.
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
- **`infer.ask.max_output_tokens`** defaults to 4096 and comes out of
  `context_tokens`. The endpoint measures the prompt and this ceiling against
  one window, so `ask` reserves it and packs excerpts into the remainder:
  raising it buys longer answers by showing the model fewer of them. It never
  reserves more than half the window — a ceiling as wide as its context would
  otherwise leave nothing to pack and answer nothing at all — and the answer
  says when it was cut off at the ceiling.
- **`infer.*.ceiling_param`** — which name the endpoint takes the output
  ceiling under, `max_tokens` or `max_completion_tokens`. Left unset it is
  inferred from `reasoning_effort` and corrected from the endpoint's own 400.
  Set it when the endpoint ignores what it does not recognise rather than
  refusing it — a llama.cpp or vLLM build with `reasoning_effort` set is the
  case that needs it, since an ignored ceiling is no ceiling at all.
- **`timeout_secs`** defaults to 900. Absurd for a hosted API, about right for a
  local model answering in minutes.

`config.example.toml` carries the same keys with the reasoning behind each
default.
