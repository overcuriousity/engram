<img src="assets/wordmark.svg" alt="engram" width="236">

**A trace of everything worth keeping.**

A self-hosted knowledge base you search by meaning. Paste text, a link, a PDF or
a photo; engram stores it verbatim, splits and embeds it, and returns the
passages that answer you. Three doors, one backend: the web UI, a REST API at
`/api/v1`, and an MCP server at `/mcp`.

## What it does

- **Capture** — paste, URL, PDF, photo, browser extension, Android share sheet,
  shell pipe. Originals are stored untouched; PDFs are read locally, images need
  `[infer.vision]`.
- **Search** — dense embedding plus a local BM25 vector, fused on every query.
  Dictation where `[infer.transcribe]` is set.
- **Ask** — one streamed answer across the base; abstains when the base has
  nothing and badges commands or paths no excerpt supports.
- **Judge** — verdicts under a search or an answer give recall@10 and MRR on
  Insights and drive the idle pass's ranking moves.
- **Duplicates and disagreements** — judged in the background; merges never drop
  a number or path, every action has an undo, conflicting values are both kept.
- **Memory that learns** — links from co-retrieval and decaying accessibility;
  the idle pass integrates, rehearses and takes back its own mistakes.
- **Reap** — artifacts retired for 90 days are rewritten or buried in a graveyard
  table (`reap.enabled`).
- **Time** — reminders and dates read at capture; due reminders push to Gotify or
  UnifiedPush.

A capture becomes a **note**; it may also be a **reminder** (asks to be
reminded), an **event** (mentions a date) or a **link** (restates a kept note).

## Requirements

Rust 1.94+, Qdrant (REST API), and an OpenAI-compatible endpoint for chat and
embeddings.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/overcuriousity/engram/master/install.sh | sh
```

Checks the release's `SHA256SUMS` and never uses sudo. `ENGRAM_INSTALL_DIR` sets
the target, `ENGRAM_VERSION` pins a tag. Or build: `cargo build --release`.

## First run

Qdrant must answer on the configured address (`127.0.0.1:6333`) first.

```bash
cp config.example.toml config.toml
engram --hash-password 'your password'   # paste into config.toml
engram
```

Open <http://127.0.0.1:8080/auth/login>. `--config` takes a path; otherwise
`config.toml` in the working directory is read if present. `--print-config`
prints the resolved configuration, redacted.

`auth.mode = "local"` binds to loopback only; anything reachable from another
machine wants `oidc`.

### As a service

`engram.service` is shipped in the release archive: binary at
`/usr/local/bin/engram`, config and data under `/var/lib/engram`, secrets in
`/var/lib/engram/engram.env`.

### One-shot commands

Each requires `--user <SUBJECT>` and, where it writes, a stopped server.

- `--reindex` — copy every vector into a fresh collection generation and swap
  the alias.
- `--recompute-coverage` — re-measure corpus coverage from stored artifacts.
- `--merge-orphans [--dry-run]` — join passages under 40 non-whitespace
  characters into their neighbour (engram#61).

## Multiple users

Every user gets their own SQLite database and Qdrant collection; settings and
`server.workers` stay instance-wide. Set `auth.mode = "oidc"`: an unseen subject
is provisioned on first sign-in if it matches `allowed_subs`, `allowed_emails` or
`allowed_groups`. Empty allow-lists are refused unless `open_registration = true`.

```bash
engram --list-users                      # subject, slug, email
engram --delete-user  sub-abc123         # row, credentials, file and alias; needs a stopped server
```

### Backup

```bash
engram --backup /srv/backup/engram-2026-10-17            # online
engram --backup /srv/backup/engram-2026-10-17 --offline  # point-in-time; refuses while running
engram --backup-check  /srv/backup/engram-2026-10-17     # exit 0/1
engram --verify-stores [--user SLUG]                     # read-only
engram --restore /srv/backup/engram-2026-10-17 [--user SLUG] [--reembed] [--force-version]
```

A backup directory holds `control.db`, `tenants/<slug>.db`,
`vectors/<alias>.snapshot` and `manifest.json` (written last, with hashes and
counts). Online backups are reconcilable: rows without points are re-embedded
at restore. Restore refuses on a bad manifest, a running server, a newer
format, a different embedding recipe or vector backend, or an unknown `--user`,
and ends with **PASS** (0), **REPAIR** (2) or **FAIL** (1). The previous Qdrant
generation is kept as the rollback.

The archive is the instance: it is created `0700`, files `0600`. Restore only
archives you made; encrypting and shipping them is yours to do.

## Clients

- **Command line** — the same binary, talking to a running engram over HTTP;
  reads `~/.config/engram/cli.toml` (written on first run, `0600`).

  ```bash
  engram -c notes.pdf                 # capture; `pbpaste | engram` captures a pipe
  engram -s 40 "loop device"          # search
  engram -a "how did I mount it?"     # stream an answer
  engram -r "call the bank tomorrow"  # hint: a reminder
  engram --show 3                     # the third hit of the last search
  ```

  Exit `1` means nothing was found.
- **Android** — a client of `/api/v1`, or a contained server on the phone;
  pairs by QR. [android/README.md](android/README.md); the APK is on the
  [releases page](https://github.com/overcuriousity/engram/releases).
- **Browser extension** — Chrome and Firefox. [extension/README.md](extension/README.md).
- **iOS Shortcut** — a token from `/extension/install`.
- **MCP** — `/mcp` with an API token.

## Configuration

[`config.example.toml`](config.example.toml) lists every key. Any key can be set
by environment variable: `ENGRAM__` prefix, `__` between levels. Set
`learn.mode` (`off`, `learning`, `full`) first. `infer.embed.dim` must match the
collection.

## Docs

- [docs/api.md](docs/api.md) — the `/api/v1` contract.
- [docs/evaluation.md](docs/evaluation.md) — how retrieval is measured.
- [android/README.md](android/README.md)
- [extension/README.md](extension/README.md)
