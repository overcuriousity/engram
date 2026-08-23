# The box is the file's annotation, and an annotation is findable — Design

Date: 2026-08-23
Status: implemented (2026-08-23) — §3 corrected during implementation; see “What changed” at the end
Touches `src/core/ingest.rs`, `src/infer/prompt.rs`, `assets/app.js`,
`src/web/templates/workspace.html`.
No new endpoint, no new table, no new model call, no change to any of the three
upload doors' wire format.
See §9 for what it is not allowed to break.

## 1. Why

Two problems that turn out to be one.

**The workspace does not say what text plus a file means.** Type into the box,
attach a PDF, press Capture, and `assets/app.js:1159-1163` runs two captures:
`postText()` stores the box as its own text artifact, then `send(file)` uploads
the file with the *separate* `input[name=note]` (`workspace.html:81`) as its
annotation. One press, two artifacts, two places to type, and no way to tell
from the page which of the two boxes the words in front of you belong to.

**A note is not findable.** Embedding runs over artifact chunks — title and
text, `src/jobs/embed.rs:18-50`. `metadata["note"]` is in neither. So:

| Door | Where the note goes | Retrievable? |
|---|---|---|
| Image | vision prompt (`prompt.rs:1312`) → description → chunks | Only as the model's paraphrase, never the operator's words |
| PDF | `metadata["note"]`, and no model in the path | **No** |
| `.txt` upload | `metadata["note"]` | **No** |

The sentence most worth searching on — *"scan of the Reinhardt lease, the break
clause is p.3"* — is the one sentence engram cannot find, on exactly the
captures whose extracted text is least likely to contain the operator's words
for it. A scanned PDF with no text layer parks as `failed` and keeps nothing
searchable at all, though somebody sat and typed what it was.

Fixing the first without the second is a regression: it would demote text that
today becomes a searchable artifact into a caption that never can be.

## 2. What is built

1. **A staged file makes the box the file's note.** The dedicated note input is
   removed. One press, one capture: the file, annotated with what is in the box.
2. **Order-independence falls out.** Nothing moves at attach time; the box's
   value is read at press time. Type-then-attach and attach-then-type reach the
   same single annotated file.
3. **While a file is staged the box goes quiet** — no search on keystroke, Ask
   disarmed, placeholder swapped. Removing the file restores it, with the text
   still in it.
4. **A note becomes a real artifact** on the file's corpus: embedded, searchable
   in the operator's own words, anchored to no line of the document.
5. **The 2000-character cap moves to the only place it earns its keep** — the
   vision prompt. A note is no longer truncated on the way into storage.

## 3. The note as an artifact

One artifact, written at capture time, on the file's own corpus:

| Field | Value | Why |
|---|---|---|
| `corpus_id` | the file's | The note points at the file; deleting the file takes it |
| `provenance` | `Note` | A new variant. See “What changed” — `Captured` did not survive implementation |
| `corpus_span` | `None` | **The load-bearing choice.** See below |
| `segment_idx` | `None` | It belongs to no window |
| `ordinal` | `0` | Renumbered to 0 anyway; see below |
| `text` | the note, trimmed | Uncapped |
| `title` | `None` | A heading is something the document gave; this had none |

`corpus_span` is already `Option<CorpusSpan>` (`artifacts.rs:135,217`), and the
production readers already treat it as optional — `lineage.rs:102` carries it
through as an `Option`, `ask/mod.rs:647` short-circuits on `?`. Every
`.expect("every artifact keeps a span")` in the tree is inside a `#[cfg(test)]`
module (`synthesize.rs:1154,1294`, all past the `mod tests` at line 304).

So the note does **not** get spliced into the document's text. The anchoring
invariant — every artifact reads beside the lines it came from — only ever
covered artifacts that *claim* a span. A span-less artifact claims none, and is
honest: it is about the file, not from it.

**Ordinals need no change at all.** (This held.) `renumber_artifacts` orders by
`COALESCE(segment_idx, 0), ordinal, rowid` (`artifacts.rs:918`). The note has
`segment_idx = None` → sorts as segment 0; `ordinal = 0`; and it was inserted
before any passage, so its `rowid` is lowest. It lands at ordinal 0, first in
reading order, and the document's passages renumber after it. Neither
`passages.rs:169` nor `window.rs:537` changes, and neither does the renumberer.

**Length needs no cap.** `embed.rs:99-131` splits an oversized chunk into
siblings — on its own estimate before the call, and again on the endpoint's
refusal. A long note becomes several chunks, the way a long paste already does.

**A parked capture keeps its note.** The artifact is written at ingest, not
after extraction, and ingest explicitly arms
`rearm_idle_seq(Stage::Embed, "corpus", corpus_id, 0)` — the same corpus-level
embed job `synthesize.rs:191` uses. A scan that `extract::park_failed` marks
`Failed` has never reached `settle`, so without that arming its note would sit
`pending` forever. With it, the one thing anybody typed about an unreadable
scan is searchable.

### Where it is written

One private helper in `ingest.rs`, called from all three doors after the corpus
row exists: `ingest_capture` (the `.txt` upload, via `with_note`, `ingest.rs:125`),
`ingest_pdf` (`ingest.rs:334`) and `ingest_image` (`ingest.rs:417`). One place,
so a fourth door cannot forget it.

`metadata["note"]` stays exactly where it is and keeps its job: the corpus page
reads it, and `describe_context` builds the vision prompt from it. It is now a
second copy of text that also lives in an artifact. That is the cheaper of the
two options — the alternative is teaching the corpus page and the describe job
to go find an artifact, which is coupling bought for nothing.

## 4. Where the cap goes

`MAX_NOTE_CHARS = 2000` currently truncates on the way into `metadata`
(`clean_note`, `ingest.rs:35`) — silently, with no receipt. It exists because a
note is "context, not a document".

That reason is now only true in one place: the vision prompt, where the note is
the *lead line* (`prompt.rs:1312-1316`) and an unbounded note would swamp the
description or overrun the call. Everywhere else the note is an artifact like
any other, and artifacts are not truncated.

So the cap moves into `describe_context`, which truncates the copy it puts in
the prompt. `clean_note` keeps the trim and the empty→`None`, and drops the
`.take(MAX_NOTE_CHARS)`. Nothing stored is truncated any more; the only bounded
copy is the one that is spent on a model call.

## 5. The box, when a file is staged

The dedicated note input is deleted from `workspace.html`. The box takes its
job:

| | Box empty, no file | File staged |
|---|---|---|
| Typing | searches (`keyup changed delay:120ms`) | nothing — no request, no embedding, no Judge row |
| Placeholder | "Describe the situation, ask a question…" | "What is it, why keep it?" |
| **Ask** | armed when the box has text | disabled |
| **Capture** | armed when the box has text | armed by the file alone |

The quiet is the point: an annotation typed into a live search box is an
embedding call, an activation bump and a Judge-queue row per phrase, for text
nobody asked as a question.

Going quiet is one attribute. The form's `hx-trigger` names
`keyup ... from:textarea[name=q]`; staging removes the box's search triggers and
unstaging restores them, alongside the `hidden` toggles `stage`/`unstage`
already do (`app.js:908-970`).

**Removing the file restores everything**, with the text untouched in the box —
Remove is the way out, and it must never be the thing that eats what you wrote.
On unstage the box's triggers come back and a search fires for whatever is in
it, so the rail matches the box again.

## 6. One press, one capture

`app.js:1159-1163` becomes: if a file is staged, send the file with
`note = box.value.trim()` and do **not** call `postText()`. With nothing staged,
`postText()` as today.

On a capture that stored, the box is cleared and the staged file released —
the same `stored` verdict on `htmx:afterRequest` that `postText` already uses
(`app.js:1120-1128`), so a failed upload leaves both the file and the words in
place. Two receipts collapse to one, and the `swap: 'beforeend'` that existed
to keep two receipts from overwriting each other is no longer load-bearing —
it stays, harmlessly, because a press can no longer produce two.

**`from_ask` is not sent with a file.** `/ui/capture?from_ask=` fills the box
with a whole model answer; attaching a file to that turns the answer into a
caption on somebody's PDF, and `origin = "ask"` is a claim about a text capture.
The `kept-from` node is removed on a stored capture as it is today.

## 7. What the operator sees

Before, two boxes and no rule:

```
┌──────────────────────────────────┐
│ ...whatever I typed up here...   │  ← searched, and captured as its own artifact
└──────────────────────────────────┘
┌──────────────────────────────────┐
│ [thumb] scan.pdf  38 KB          │
│ Note for the file (optional)…    │  ← and here too?
│                         [Remove] │
└──────────────────────────────────┘
        [ Ask ]  [ Capture ]  [ Attach ]
```

After, one box whose job the file decides:

```
┌──────────────────────────────────┐
│ [thumb] scan.pdf  38 KB [Remove] │
└──────────────────────────────────┘
┌──────────────────────────────────┐
│ What is it, why keep it?         │
│ scan of the Reinhardt lease,     │
│ break clause is p.3              │
└──────────────────────────────────┘
        [ Ask ]  [ Capture ]  [ Attach ]
         dimmed    armed
```

Press Capture: one PDF, annotated. Search "break clause lease" next month and
that sentence comes back, pointing at the scan.

## 8. Testing

Ingest (`src/core/ingest.rs`):
- a note on a PDF capture writes one span-less artifact on that corpus, ordinal
  0, provenance `captured`
- the same for an image and for a `.txt` upload — one helper, three doors
- a capture with no note writes no artifact
- a whitespace-only note writes no artifact and no `metadata["note"]`
- a note longer than 2000 chars is stored whole, in both metadata and artifact
- the note artifact is `pending` and a corpus Embed job is armed

Ordering (`src/store/artifacts.rs` or `src/jobs/passages.rs`):
- after passages land and `renumber_artifacts` runs, the note is ordinal 0 and
  the document's first passage is ordinal 1

Parked capture (`src/jobs/extract.rs`):
- a PDF that `park_failed` marks `Failed` still has its note artifact, and it
  still reaches `embedded`

Prompt (`src/infer/prompt.rs`):
- `describe_context` truncates a 5000-char note to `MAX_NOTE_CHARS`
- the note still leads, ahead of the capture facts (the existing test at
  `prompt.rs:2261` must keep passing)

Retrieval (`src/core/search.rs`):
- a search for the note's wording returns the note artifact, and it carries the
  file's `corpus_id`

Front end: exercised by hand — the automated suite does not drive `app.js`.
§9 is the checklist.

## 9. What it must not break

- **Order-independence.** Type→attach→Capture and attach→type→Capture give the
  same single annotated file. Nothing may move text at attach time.
- **Remove gives the text back.** Unstaging restores the box's search triggers,
  the placeholder, Ask, and leaves the typed text exactly as it was.
- **A failed capture keeps everything.** No box cleared, no file released,
  unless the `htmx:afterRequest` verdict says stored.
- **A capture with no file behaves exactly as today** — search on keystroke,
  Ask armed, `from_ask` carried, box cleared on success.
- **The three upload doors' wire format is unchanged.** `note` is still an
  optional multipart field on `/api/v1` (`api.rs:364,470`); the API and MCP
  callers gain searchable notes without changing a line.
- **The image path still reads the note before the pixels.** `describe_context`
  keeps the note as its first line.
- **No new model call.** The note is embedded by the corpus Embed job that the
  capture already arms; at `synthesis = "off"` and `"earned"` nothing else runs.
- **Deleting the file takes the note.** It is an artifact on that corpus and
  goes with it.
- **A photo still waits for Capture.** The camera handing back a file stages it;
  it does not upload.


---

## 10. What changed during implementation

Two things this spec asserted did not survive contact. Both were found by the
tests it asked for, and both are now in the code.

**`segment_idx: None` was not a free marker.** The column already means
something for a window-less row: *debris from an older segmentation*.
`artifact_ids_for_segment` (`src/store/artifacts.rs:884`) matches
`segment_idx = ? OR segment_idx IS NULL` deliberately, so the sweep before a
window rewrite would have deleted the note — and, worse, `passages.rs:159`
read window 0 as already written and **skipped every passage**. A file
captured with an annotation was never chunked at all. A sentinel index instead
of `NULL` fails the other way: `upsert_segments`
(`src/store/segments.rs:115-121`) refuses to segment a corpus any artifact owns
a window in. Provenance could not distinguish it either —
`write_segment_artifacts` writes through `insert_artifacts`, whose default is
`Captured`.

So there is a fourth `Provenance`: `Note`. The distinction lives in the field
that means *what kind of row is this*, and the two queries above exclude it.
`renumber_artifacts` needed no change — that half of §3 held exactly as
written.

**`settle_corpus` read "nothing left to embed" as "finished".** That is only
true for a source whose windows have been written: `synthesize::finish` sets
`embedding` *before* arming the job. §3 arms the note's embed at the door
instead, so the note embedding alone would have found nothing else pending and
reported a PDF `ready` before it was extracted — and walked a parked `failed`
scan forward to `ready` on the way past. It now returns early unless the corpus
is `embedding` or `partial`. The bug was latent before this work: nothing could
arm an embed outside that window, so nothing could reach it.

**Two effects accepted rather than fixed.** A note is `in_results()`, so it is
eligible for dedupe and merge like any other captured text — two identical
notes on two files can consolidate. And `Provenance::parse` maps an unknown
string to `Captured`, so a `note` row read by an older binary is swept as
debris: this is forward-safe, not rollback-safe.

**One §7 sketch was wrong about the DOM.** The staged file sits *below* the
box, not above it. Nothing was moved; the mockup drew it the other way round.
