# engram — Usable today

Date: 2026-08-09
Status: approved
Builds on `2026-08-09-engram-design.md`. Implements the first slice of
`ROADMAP.md`.

## 1. Scope

One branch, four strands, one theme: a capture you can trust, and a result you
can act on.

| Strand | Why it is in |
|---|---|
| Per-window segmentation | One bad window must not discard a whole source |
| Fidelity verification | Nothing checks that literals survived the rewrite |
| Search workspace | A ranked chunk is not yet the command you run |
| Typing behaviour | Every keystroke costs an embedding and drains `resurface` |

The expected shape of use is a chapter at a time, not a book at a time. The
capture screen says so, and the design optimises for that case; a book-sized
paste must still complete, but it is the edge case, not the target.

### Out of scope

File upload, PDF ingest, facets, filter chips, near-duplicate detection,
related chunks, an evaluation harness, reranking defaults, server-side
grouping, the FTS5 decision, term-id collisions, multi-user. Each is its own
branch. Section 4 defines the one seam PDF will later need.

## 2. Segmentation that fails per window, not per source

### Today

`segment::run` windows the raw text and loops the windows inside a single job.
Two consequences, both bad for anything longer than a page:

- A retry re-runs every window from the first, re-paying every completed call.
- A window that exhausts `MAX_ATTEMPTS` sends the **whole source** through
  `run_with_fallback`, discarding good LLM segmentation for every other window.

### Design

Window results persist as they complete.

```sql
-- migration 0005
CREATE TABLE segment_windows (
  source_id   TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
  idx         INTEGER NOT NULL,
  start_line  INTEGER NOT NULL,
  end_line    INTEGER NOT NULL,
  state       TEXT NOT NULL,      -- pending | done | fallback
  attempts    INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  PRIMARY KEY (source_id, idx)
);
ALTER TABLE chunks  ADD COLUMN window_idx INTEGER;
ALTER TABLE sources ADD COLUMN coverage REAL;
ALTER TABLE chunks  ADD COLUMN flags TEXT;         -- JSON array, NULL when clean
ALTER TABLE chunks  ADD COLUMN flag_detail TEXT;   -- human-readable, one line
```

The segment job becomes: split into windows, upsert the window rows once, then
process every window whose state is `pending`.

- **Resume.** A retried job skips windows already `done` or `fallback`. The
  windows table is the job's memory; the job row stays as it is.
- **Per-window failure.** A window past `MAX_ATTEMPTS` gets `structural_chunks`
  over *its own lines only* and is recorded `fallback`. Other windows keep
  their LLM chunks. The source ends `partial`, and Ops names the window with
  its line range and last error.
- **Narrower replace.** `write_chunks` today deletes every chunk of the source
  before inserting. It becomes: delete the chunks of *this window* (SQLite by
  `window_idx`, Qdrant by a `window_idx` payload filter), then insert. The
  "replace, never append" guarantee is unchanged; its key is narrower.
- **One embed job per source, still.** The embed job is enqueued once, after
  the last window resolves, so embedding remains a single call for the source.
- **Progress.** The source exposes `windows_done / windows_total`. Browse and
  the capture confirmation render `segmenting 3/9` rather than a status that
  does not move for minutes.

### Capture ergonomics

- The capture screen states the expectation: paste a chapter, not a book.
- Above roughly one window's worth of text, a soft warning appears with the
  window count the paste will cost. It is advice, not a block.
- The request body limit is set explicitly to 8 MB rather than inheriting
  axum's 2 MB default, and an oversize capture is refused with a message that
  names the limit instead of failing as a truncated form.

## 3. Fidelity verification

Runs per window, after the chunker returns and before those chunks are
written.

### Literal check

From each proposed chunk, extract:

- lines inside fenced code blocks,
- inline code spans,
- path-like and flag-like bare tokens: leading `/`, `--`, `~/`, or a token
  containing `/` with no whitespace.

Every extracted literal must appear in the raw text of the window it came from.
Comparison normalises whitespace runs to a single space and ignores leading
indentation; it is otherwise exact — a changed flag is a mismatch.

On mismatch: re-segment that window once at temperature 0. If the retry is
clean, use it. If it mismatches again, store the chunks and mark each offender
`literals_unverified`, recording the first offending literal in `flag_detail`.

Rationale: a paraphrased command is a command that gets pasted into a root
shell. Storing it silently is the failure this project cannot afford; refusing
to store it at all loses a chapter to one bad line, which is worse than a
visible warning.

### Span check

`source_lines` comes back from the model and is trusted after the window offset
is applied. A span is accepted when it lies within its window and the chunk's
text shares plausible token overlap with the lines it claims (at least a third
of the chunk's distinctive tokens appear in the claimed range). Failure clamps
the span to the window bounds and marks the chunk `span_unverified`.

The detail pane in section 4 renders the claimed lines beside the chunk, so a
wrong span is not cosmetic — it makes the pane show the wrong text. Marking it
is what stops that from reading as a rendering bug.

### Coverage

Per source, the fraction of raw non-blank lines covered by at least one chunk
span, computed once every window has resolved and stored on the source. Browse
shows it; below 0.6 it renders as a warning. A source where the segmenter
dropped half the chapter currently looks identical to one where it did not.

### Surfacing

- Flag badge on the rail entry, the result card, and the detail pane.
- The detail pane's flag banner names the offending literal and offers
  **Re-segment window** (resets that window to `pending`, re-enqueues the job)
  and **Mark reviewed** (clears the flag, records nothing else).
- Ops lists flagged chunks and low-coverage sources alongside failed jobs.

## 4. Search workspace

`/ui/search` becomes a two-region workspace: the search box spans the top and
stays, a ranked rail sits on the left, and the chunk detail pane fills the
rest.

```
┌──────────────────────────────────────────────┐
│ [ dd write iso to usb            ]  ⌕        │
├───────────────┬──────────────────────────────┤
│ 1 Writing an  │ Writing an ISO … with dd     │
│   ISO with dd │ ┌──────────┬───────────────┐ │
│ 2 Verifying   │ │ chunk    │ source 118-141│ │
│   the write   │ │ text +   │ 124  umount … │ │
│ 3 Secure Boot │ │ code     │ 125  dd if=…  │ │
│ 4 fdisk …     │ └──────────┴───────────────┘ │
│ 5 …           │ tags: procedure linux dd     │
└───────────────┴──────────────────────────────┘
```

- **Rail entry**: position, title, source name, a clamped one-line snippet, and
  a flag badge when the chunk carries one. `↑`/`↓` move the selection, `Enter`
  opens it.
- **Pane**: chunk on the left, the source lines it claims on the right, the
  layout approved in the mockup. Flag banner above when flagged.
- **Routing**: `GET /ui/chunks/{id}` returns the pane fragment for an
  `HX-Request` and a standalone page otherwise, so an htmx swap and a pasted
  link produce the same view. The rail swap uses `hx-push-url`, and the query
  string carries the query so a reload restores rail and pane together.
- **Snippet clamp** on rail entries and result cards: bounded height, a fade,
  and an expand control. Clamping is visual only — text is never truncated
  server-side, so a fenced block is never cut mid-command.
- **Copy button** on every fenced block in the pane and in expanded cards.
- **Highlighting** is client-side: the response carries the query's indexable
  terms (the same terms the sparse branch derives), and a small script wraps
  matches in text nodes only. `ammonia`'s output stays the last word on what
  HTML exists in a rendered chunk.
- **Narrow screens**: one region at a time. The rail is the list, opening a
  chunk replaces it, back returns to it. No two-pane layout on a phone.

### The source view seam

The right pane does not read raw text directly. It asks a `SourceView` for the
lines around a span, with the span marked:

```rust
pub struct SourceSlice {
    pub lines: Vec<(i64, String)>,  // line number, text
    pub span: (i64, i64),           // the marked range
    pub label: String,              // "lines 118–141"
}

pub trait SourceView {
    fn slice(&self, source: &Source, span: SourceSpan, context: usize) -> SourceSlice;
}
```

One implementation ships: `TextLines`, over `sources.raw_text`. PDF later
implements the same trait — its `label` reads `page 42`, and its slice may be
rendered rather than textual. That is the entire reason the seam exists, and
nothing else in the pane needs to know which implementation answered.

## 5. Typing

- **Query embedding cache.** A bounded LRU in `Core`, roughly 256 entries,
  keyed on the normalised query plus the embed model id. Prefixes repeat
  constantly inside one search and identical queries repeat across sessions;
  this removes the dominant latency term before Qdrant is touched at all.
- **`mark_seen` only on intent.** `Core::search` takes an explicit `mark`
  flag. Incremental UI requests pass `false`; opening a chunk, expanding a
  card, and submitting the box pass `true`. `/api/v1/search` and the MCP tool
  keep marking — those calls are deliberate by construction. Without this,
  typing `d`, `dd`, `dd if` stamps `last_seen_at` on whatever each prefix
  matched, and `resurface` is quietly drained by the act of searching.
- **Latency, faintly.** `embed 41ms · total 138ms` under the rail, from the
  numbers `search` already computes. It tells you whether a sluggish box is
  the embedder, the vector store or the reranker without opening logs.

## 6. Testing

All of it runs against the existing fakes and the in-memory vector store. No
new infrastructure.

**Segmentation**

- A chunker that always fails on window 3: windows 1, 2 and 4 keep LLM chunks,
  window 3 gets structural chunks covering its lines only, source is `partial`.
- Re-running the job after a partial failure does not duplicate chunks and does
  not re-call the chunker for windows already `done`.
- The embed job is enqueued exactly once per completed segmentation.

**Fidelity**

- A chunker that drops `oflag=sync` from a command triggers exactly one
  re-segmentation, then stores the chunk flagged, with that literal in
  `flag_detail`.
- A chunker whose retry is clean stores no flag.
- A chunker returning `source_lines` outside its window has the span clamped
  and the chunk flagged.
- Coverage over known spans matches the hand-computed fraction; a source with
  half its lines unclaimed lands below the warning threshold.

**Workspace**

- `/ui/chunks/{id}` returns a fragment under `HX-Request` and a full page
  without it.
- The source slice contains the claimed lines plus context, with the span
  marked.
- A chunk whose source was deleted does not 500.

**Typing**

- Two identical queries produce one embed call; a third with different
  whitespace still hits the cache.
- An incremental search leaves `last_seen_at` untouched; opening a chunk sets
  it.

## 7. Order of work

1. Migration `0005`, windows table, per-window segmentation with resume.
2. Per-window fallback, progress fields, capture guidance, explicit body limit.
3. Literal, span and coverage verification, plus the flag surfaces on Ops.
4. `/ui/chunks/{id}`, the `SourceView` seam, the rail-and-pane workspace.
5. Snippet clamp, highlighting, copy buttons.
6. Embedding cache, `mark` flag, latency line.

Each step leaves the tree green and the application usable. Steps 1–3 are
worth deploying before 4–6 exist; the reverse is not true, because a workspace
over unverified chunks is a nicer way to read something that may be wrong.
