# PDF ingest

engram reads text, links and photographs. It does not read the format most of
the documents worth keeping actually arrive in. The upload door says so out
loud — `src/web/api.rs`, above `upload()`:

> `.txt` and nothing else, for now. PDF is a `SourceView` implementation and a
> later plan; refusing everything else by name is what keeps this one from
> quietly ingesting the bytes of a format it cannot read.

This is that later plan.

## What this is not

The idea that started it was a Rust port of
[pdf2epub](https://github.com/overcuriousity/pdf2epub) without the EPUB half.
That is not available: `modules/pdf2md.py` is 172 lines of glue, and everything
underneath it is **marker-pdf** — a PyTorch and transformers stack of Surya
layout, OCR and table models. Porting it is not a feature, it is a second
project.

Two other premises also turned out to be already satisfied. Markdown rendering
in the frontend exists: `src/web/markdown.rs` renders artifacts through
`pulldown-cmark` and sanitizes with `ammonia`. And "markdown internally" is
mostly the status quo already — fetch converts through `html2md`, the vision
path writes markdown, artifacts are markdown. Only pasted text is raw, and it
should stay raw, because it is the verbatim source everything else traces back
to.

What is left is the actual work: a PDF door.

## The engine

[`docling`](https://github.com/docling-project/docling.rs) — MIT, the official
docling organisation, a Rust port of IBM's document converter. It depends on
`pulldown-cmark 0.13.4`, the version already in this tree. It offers exactly
two rungs, and choosing between them is the whole design decision:

| | `pdf-text` | `pdf` (ML) |
|---|---|---|
| dependencies | `docling-core`, `lopdf` | plus `ort` (ONNX Runtime), `pdfium-render`, `tokenizers` |
| models | none | layout, TableFormer, OCR — fetched by a download step or `$DOCLING_RS_MODELS_URL` |
| deployment | one static binary stays one static binary | native libraries; prebuilt pdfium is Linux x64 only |
| multi-column, tables | geometric reconstruction only | what marker exists for |

**engram builds `pdf-text` by default and puts the ML rung behind a cargo
feature.** The default build gains no models, no ONNX runtime and no native
libraries; `--features pdf-ml` turns on the rest for anyone who wants it. One
function sits in front of both, and the job stage cannot tell them apart.

This is a bet with a way out, which is the point. docling 1.14.0 was published
on 2026-08-19 and the repository is weeks old. The version is pinned exactly,
the risk is written into `Cargo.toml` the way `html2md`'s and
`dom_smoothie`'s are, and the fallback if the crate becomes unacceptable is
`pdf-extract` — which costs the splitter its heading boundaries, and that
sentence belongs in the comment.

One cost is not avoidable and should not be discovered in a build log:
`docling`'s non-optional dependencies come along without ML. `calamine`,
`scraper`, `zip`, `quick-xml`, `roxmltree`, `mail-parser`, `rayon`, `csv`,
`snap`, `image` — roughly fifteen new crates and their trees, for one door.

## Provenance: lines, labelled as extracted

A PDF corpus's `raw_text` is the markdown docling produced. Spans into it are
line spans, exactly as everywhere else, and `CorpusSlice.label` reads
`extraction` — the same move the image arm already makes with `transcription`,
and for the same reason:

> a span into a transcription is a claim about what the model wrote, not about
> what the photo shows

A span into an extraction is a claim about what docling wrote, not about what
the PDF laid out. The label says so, and no new state exists to say it.

`page 42` is **rejected**. It reads better and it costs more than it returns:
`CorpusSpan` is `start_line`/`end_line`, the splitter counts lines, and every
stored span in the base counts lines. A line-to-page map alongside them would
work, but it is state that exists only to improve a label, and it bets on
docling's Rust API exposing `prov.page_no` — a bet with no payoff worth making.

Two places in the repository promise `page 42` today and this plan must
retract both:

- `ROADMAP.md`, the **PDF capture** bullet under *Core Platform & Tooling*
- `src/web/corpus_view.rs`, the module doc: *"A PDF source would be one more
  arm of `slice`, its label reading `page 42`."*

Leaving either in place would leave a false statement in the tree.

## The path a PDF takes

The image door is the template at every step, because a PDF is the same shape
of thing: bytes that are not text, kept verbatim, turned into markdown by a
stage that runs later and can be run again.

**Storage — no schema change.** `attachments` already says of itself "one row
per image corpus **today**", and it carries `kind` and `mime`. A PDF is
`kind = "pdf"`, `mime = "application/pdf"`, `bytes` the original untouched,
`preview` empty. A first-page thumbnail would need pdfium, so it arrives with
`pdf-ml` or not at all.

**Ingest — `core/pdf.rs`, built after `core/image.rs`.** `ORIGIN_PDF = "pdf"`
joins the origin constants in `core/ingest.rs`: how a document got here is the
only record of how a document got here, and a PDF is not an upload of text.
`ingest_pdf` hashes the bytes before doing anything else — a PDF sent twice
costs one SHA-256 the second time, not an extraction — then writes the corpus
and its attachment and queues the extraction.

Unlike the image door there is no gate: `pdf-text` makes no inference call, so
PDF works with no configuration at all, where images need `[infer.vision]`.
Also unlike the image door, no decode permit is needed — the text parser is far
cheaper than a pixel decode. `spawn_blocking` still is: `lopdf` walks up to
`pdf_max_bytes` synchronously, and held on a Tokio worker that is seconds
during which search, health and the queue poll on that thread all wait. The
reasoning is already written out above `web::api::extract`.

**The job — `Stage::Extract`, `jobs/extract.rs`, after `jobs/describe.rs`.**
Local work, no model call. On success the markdown becomes `raw_text` and the
corpus hands off to `Synthesize`, from where it is an ordinary corpus. On
failure the corpus is `failed` with the reason on its page, and
`POST /api/v1/corpora/{id}/reprocess` with `{"stage":"extract"}` reads it again
from the stored bytes.

That last sentence is what makes the ML rung redeemable rather than
theoretical: turn the feature on, press **Re-read**, and the same PDF becomes
better markdown with no re-upload and no lost provenance.

## Size

`[capture] pdf_max_bytes = 52428800`, following `image_max_bytes` exactly —
its own `DefaultBodyLimit` on the upload route, above the global 8 MB in
`web::mod::MAX_BODY_BYTES`.

Nothing else bounds it. A 600-page book becomes hundreds of segments and
hundreds of model calls, and that is allowed: the job queue is already
throttled and resumable, `[infer.budget]` already exists, and feeding a book to
engram is a deliberate act. No page cap, and no page-range field on the upload
— nobody has wanted either, and both are cheap to add the day someone does.

## Web

- **`upload()`** gains `named_pdf` beside `named_txt`, and `application/pdf` in
  the content-type branch. The route gains
  `DefaultBodyLimit::max(pdf_max_bytes)`.
- **`capture.html`**: `accept` widened to include `.pdf,application/pdf`, and
  the wording beside the drop target updated. The page's JavaScript already
  routes by MIME into either the `image` or the `file` part, so a PDF lands on
  `file` without touching that logic.
- **The original**, served at `GET /api/v1/corpora/{id}/file` — a generic
  sibling of the image door's `?original=1`, answering with the attachment's
  own mime. The image route stays exactly where it is.
- **`slice()`** gains a third case. The `transcript: bool` becomes a label
  function over three origins: corpus, transcription, extraction.
- **Markdown rendering: nothing to do.** It has been there all along.

## Testing

- A small committed fixture PDF — a heading and a paragraph, a few kilobytes —
  extracts to markdown whose heading is a `#`.
- The upload door has its own larger body limit, after
  `the_image_door_has_its_own_larger_body_limit`.
- `named_pdf` accepts `.pdf` case-insensitively and refuses everything else,
  after the `named_txt` tests.
- Garbage bytes under `application/pdf` leave the corpus `failed` with a
  reason, and `reprocess` runs the stage again.
- `slice()` labels an `ORIGIN_PDF` corpus `extraction`.

## Out of scope

DOCX, EPUB, XLSX and the rest of docling's format list — nearly free once the
crate is in the tree, and deliberately not now. Page ranges at upload. Page-based
spans. OCR. PDF over MCP. `pdf-ml` on by default.

## The first step is a spike

Before any integration code: run three to five **real** PDFs through
`docling-cli` built without the ML rung and read the markdown. (The exact
feature spelling on the CLI crate is unverified — check it there rather than
assuming it mirrors the library's `pdf-text`.)

The entire choice of rung rests on the assumption that the plain text parser is
good enough on multi-column, table-bearing documents. If it is not, the answer
is not a new design — it is `--features pdf-ml`. But that is worth knowing
before the default build is settled, not after.

### What the spike found

Run on two real documents — a 58k-character software manual and a 24k-character
course guide — the `pdf-text` rung extracted **complete text in correct reading
order and not one heading or table row**:

| | characters | headings | table rows |
|---|---|---|---|
| a software manual | 58,402 | 0 | 0 |
| a course guide | 23,781 | 0 | 0 |

That is not a tunable. Heading detection *is* the layout model, which is what
the `ml` feature gates; no converter option recovers it. HTML entities also
survive into the output unescaped (`&amp;`).

**The decision was to ship it anyway.** `src/infer/split.rs` prefers headings,
then blank lines, then a hard cut — so ingest works, on the blank-line
fallback. What is lost is the heading each window after the first carries over,
which is how a procedure split across two windows tells the model what it
belongs to. On a long structured document the artifacts are therefore weaker
than the same document pasted as text with its markdown intact.

`--features pdf-ml` remains the answer for anyone who wants the structure, and
the README says plainly that the default build does not recover it.
