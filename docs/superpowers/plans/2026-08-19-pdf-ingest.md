# PDF Ingest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A PDF dropped on the capture page is stored verbatim, turned into markdown by a local stage that can be run again, and from there is a corpus like any other.

**Architecture:** A PDF takes the image path exactly. Bytes go into `attachments` beside a corpus that starts in a new `extracting` status; a new `Stage::Extract` job converts them with `docling` and writes `raw_text`; `Synthesize` takes over. No model call, no schema change, no new provenance. The machinery the image door already owns — attachment insert, read-text write, park-with-reason, re-read — is widened from "image" to "attached file" and shared, rather than copied.

**Tech Stack:** Rust 2024, `docling` 1.14.0 (`pdf-text`, no ML), sqlx/SQLite, axum, askama, the existing job queue.

**Spec:** `docs/superpowers/specs/2026-08-19-pdf-ingest-design.md`

## Global Constraints

- Dependency, verbatim: `docling = { version = "=1.14.0", default-features = false, features = ["pdf-text"] }`. Pinned exactly — the crate is days old.
- New cargo feature on engram: `pdf-ml = ["docling/pdf"]`, **off by default**. No task turns it on.
- Config, verbatim: `[capture] pdf_max_bytes = 52428800` (50 MB). No page cap, no page-range field.
- Names, fixed across every task: `CorpusStatus::Extracting` (`"extracting"`), `Stage::Extract` (`"extract"`), `ORIGIN_PDF = "pdf"`, `core::pdf::to_markdown`, `core::ingest::PdfCapture`, `Core::ingest_pdf`, `jobs::extract::{run, park_failed}`, metadata key `metadata["extract"]["error"]`, corpus-slice label `extraction`.
- Renames locked in Task 2 and used by every later task: `NewImage` → `NewFile`, `insert_image_corpus` → `insert_attached_corpus`, `set_described_text` → `set_read_text`, `clear_described_text` → `clear_read_text`.
- The image path's behaviour must not change anywhere in this plan. The existing image tests are the regression net; if one needs editing beyond a rename, stop and say so.
- `page 42` is not implemented. Two files promise it and Task 7 retracts both.
- Every task ends `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, then a commit. Commit style: lowercase type prefix, a subject that states the behaviour rather than the edit.

---

## File structure

| file | responsibility |
|---|---|
| `Cargo.toml` | the `docling` dependency, pinned, commented; the `pdf-ml` feature |
| `src/core/pdf.rs` (new) | `to_markdown(&[u8]) -> Result<String>` — the only place docling is called |
| `src/store/attachments.rs` | `NewFile` (was `NewImage`) |
| `src/store/corpora.rs` | `CorpusStatus::Extracting`; `insert_attached_corpus`; `set_read_text` / `clear_read_text` |
| `src/store/jobs.rs` | `Stage::Extract` |
| `src/core/ingest.rs` | `ORIGIN_PDF`, `PdfCapture`, `ingest_pdf`, `reprocess(Stage::Extract)` |
| `src/jobs/extract.rs` (new) | the extraction stage and its parking |
| `src/jobs/mod.rs` | dispatch and exhaustion handling for `Extract` |
| `src/config.rs`, `config.example.toml` | `pdf_max_bytes` |
| `src/web/api.rs` | `named_pdf`, the widened `upload`, the route body limit, `GET /corpora/{id}/file` |
| `src/web/corpus_view.rs` | the third label case |
| `src/web/ui.rs`, `templates/corpus.html`, `templates/capture.html` | the PDF badge, Re-read button, drop-zone type, error row |
| `ROADMAP.md`, `README.md` | the `page 42` retraction and the new door |
| `tests/fixtures/one-heading.pdf`, `tests/fixtures/no-text.pdf` (new) | the extraction fixtures: one with a text layer, one without |

---

## Task 0: Spike — is `pdf-text` good enough?

**No code is kept.** The choice of rung rests on an assumption; find out before building on it.

- [ ] **Step 1: Get the CLI**

```bash
cargo install docling-cli --version 1.14.0 --no-default-features
```

If that feature spelling is wrong, read the CLI crate's own `Cargo.toml` — do **not** assume it mirrors the library's `pdf-text`. If no non-ML build of the CLI exists, build the library's example instead, or use the browser demo at <https://docling-project.github.io/docling.rs/> with no models pointed at it.

- [ ] **Step 2: Convert three to five real PDFs**

Use documents like the ones this base will actually hold — at least one multi-column paper and at least one with a table.

- [ ] **Step 3: Read the markdown and judge two things**

1. Is reading order right on the multi-column pages, or is it interleaved into nonsense?
2. Do headings come out as `#`/`##`? The splitter cuts on headings; without them a 400-page book is one undifferentiated run of text.

- [ ] **Step 4: Record the verdict in the spec**

Append a short section to `docs/superpowers/specs/2026-08-19-pdf-ingest-design.md` under "The first step is a spike" saying what was tried and what came out. If the answer is "not good enough", say so and stop — the remaining tasks are unchanged except that `pdf-ml` becomes the default, which is a decision for the human, not for the executor.

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-08-19-pdf-ingest-design.md
git commit -m "docs: what the plain text parser actually does to a two-column page"
```

---

## Task 1: The extractor

One function, no wiring. It is the only place in the tree that names `docling`.

**Files:**
- Modify: `Cargo.toml`
- Create: `src/core/pdf.rs`
- Modify: `src/core/mod.rs` (add `pub mod pdf;`)
- Create: `tests/fixtures/one-heading.pdf`, `tests/fixtures/no-text.pdf`

**Interfaces:**
- Produces: `pub fn to_markdown(bytes: &[u8]) -> crate::error::Result<String>` — synchronous and CPU-bound; callers wrap it in `spawn_blocking`.

- [ ] **Step 1: Make the fixture**

A tiny PDF with one heading and one paragraph. Generate it, do not hand-write it — a malformed fixture makes every later failure ambiguous. If `typst` is available:

```bash
mkdir -p tests/fixtures
printf '#set page(width: 10cm, height: 6cm)\n= Ship the beta\n\nThe quarterly plan lists three goals.\n' > /tmp/f.typ
typst compile /tmp/f.typ tests/fixtures/one-heading.pdf

printf '#set page(width: 10cm, height: 6cm)\n#rect(width: 4cm, height: 2cm, fill: black)\n' > /tmp/g.typ
typst compile /tmp/g.typ tests/fixtures/no-text.pdf
```

The second fixture is a page with a filled rectangle and no text at all — the
stand-in for a scan. If your tool cannot produce one, an empty page will do;
what matters is that it carries no text layer.

Otherwise use `pandoc`, LibreOffice `--convert-to pdf`, or a browser's print-to-PDF on a two-line HTML file. Whichever you use, record it in a comment at the top of `src/core/pdf.rs`'s test module, so the next person can regenerate it.

Verify it is a real PDF before going further:

```bash
head -c 5 tests/fixtures/one-heading.pdf   # must print %PDF-
head -c 5 tests/fixtures/no-text.pdf       # must print %PDF-
```

- [ ] **Step 2: Add the dependency**

In `Cargo.toml`, under `[dependencies]`, keeping the file's habit of explaining a dependency that is not obvious:

```toml
# PDF → markdown. The layout work is docling's; the alternative is lopdf plus
# hand-written column and heading heuristics, which is marker's job description.
# Pinned exactly: 1.x is days old. The fallback if it becomes unacceptable is
# pdf-extract, which costs the splitter its heading boundaries.
#
# `pdf-text` is the pure-Rust rung: no ONNX runtime, no native libraries, no
# models to fetch. `--features pdf-ml` swaps in the layout and table models for
# anyone who wants them; nothing in this tree assumes they are there.
docling = { version = "=1.14.0", default-features = false, features = ["pdf-text"] }
```

And a `[features]` section (create it if absent):

```toml
[features]
default = []
pdf-ml = ["docling/pdf"]
```

- [ ] **Step 3: Write the failing tests**

Create `src/core/pdf.rs`:

```rust
//! PDF → markdown, and the only place `docling` is named.
//!
//! Synchronous and CPU-bound: it walks up to `pdf_max_bytes` without yielding,
//! so every caller runs it under `spawn_blocking`. See `web::api::extract` for
//! the same reasoning about `dom_smoothie`.

use crate::error::{Error, Result};

pub fn to_markdown(_bytes: &[u8]) -> Result<String> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `one-heading.pdf` was generated, not written by hand — see the plan for
    // the command. Regenerate it rather than editing bytes.
    const ONE_HEADING: &[u8] = include_bytes!("../../tests/fixtures/one-heading.pdf");
    /// A single blank page — what a scanned PDF looks like to a parser with no
    /// OCR behind it.
    const NO_TEXT: &[u8] = include_bytes!("../../tests/fixtures/no-text.pdf");

    #[test]
    fn a_pdf_becomes_markdown_carrying_its_words() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            md.contains("quarterly plan lists three goals"),
            "the body did not survive extraction: {md}"
        );
    }

    /// The canary for the whole choice of rung, not a test of our code.
    ///
    /// The splitter cuts on headings. If this fails, the plain text parser is
    /// not recovering document structure, and the answer is `--features
    /// pdf-ml` — a decision for a person, not a fix in this file.
    #[test]
    fn a_heading_comes_out_as_a_heading() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            md.lines().any(|l| l.trim_start().starts_with('#') && l.contains("Ship the beta")),
            "no markdown heading in: {md}"
        );
    }

    #[test]
    fn bytes_that_are_not_a_pdf_are_an_error_naming_the_input() {
        let e = to_markdown(b"this is not a pdf at all").unwrap_err();
        let msg = e.to_string();
        assert!(msg.to_lowercase().contains("pdf"), "unhelpful error: {msg}");
    }

    #[test]
    fn a_pdf_with_no_text_layer_is_an_error_rather_than_an_empty_corpus() {
        // A page that extracts to "" must not become a corpus with no text:
        // synthesis would then run on nothing and the failure would surface
        // three stages away from its cause. This is what a scan looks like to
        // the `pdf-text` rung.
        let e = to_markdown(NO_TEXT).unwrap_err();
        assert!(
            e.to_string().contains("no extractable text"),
            "unhelpful error: {e}"
        );
    }
}
```

Add `pub mod pdf;` to `src/core/mod.rs`, in the module list's existing alphabetical position.

- [ ] **Step 4: Run them and watch them fail**

```bash
cargo test --lib core::pdf
```

Expected: all four panic at the `todo!()`.

- [ ] **Step 5: Implement**

Read `docling`'s docs for the exact conversion entry point before writing this — the sketch below names the shape, not necessarily the API:

```rust
pub fn to_markdown(bytes: &[u8]) -> Result<String> {
    let doc = docling::DocumentConverter::new()
        .convert_bytes(bytes, docling::InputFormat::Pdf)
        .map_err(|e| Error::Validation(format!("that PDF could not be read: {e}")))?;
    let md = doc.export_to_markdown();
    if md.trim().is_empty() {
        // A PDF of scanned pages has no text layer, and `pdf-text` cannot
        // invent one. Saying so beats a corpus that is silently empty.
        return Err(Error::Validation(
            "that PDF holds no extractable text — it is probably a scan, \
             which needs the pdf-ml build"
                .into(),
        ));
    }
    Ok(md)
}
```

`Error::Validation` is the right variant: a PDF that cannot be read is the caller's input, not our fault, and the job layer must not retry it forever. Confirm against `src/error.rs` that `Validation` is non-retryable (`Error::retryable`).

- [ ] **Step 6: Run them and watch them pass**

```bash
cargo test --lib core::pdf
```

If `a_heading_comes_out_as_a_heading` is the only failure, stop and report it — that is the spike's verdict arriving late, and it is a decision, not a bug.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/core/pdf.rs src/core/mod.rs tests/fixtures/
git commit -m "feat: a PDF becomes markdown, without a model and without leaving the process"
```

---

## Task 2: Widen the image machinery to any attached file

Pure rename and generalization. **No behaviour changes.** The existing image suite is the gate.

**Files:**
- Modify: `src/store/attachments.rs` (`NewImage` → `NewFile`)
- Modify: `src/store/corpora.rs` (`insert_image_corpus` → `insert_attached_corpus`; `set_described_text` → `set_read_text`; `clear_described_text` → `clear_read_text`)
- Modify: call sites — `src/core/ingest.rs`, `src/jobs/describe.rs`, `src/web/corpus_view.rs`, `src/web/ui.rs`

**Interfaces:**
- Produces:
  - `pub struct NewFile<'a> { pub kind: &'a str, pub mime: &'a str, pub filename: Option<&'a str>, pub bytes: &'a [u8], pub preview: &'a [u8], pub width: Option<i64>, pub height: Option<i64> }` — fields unchanged, name only.
  - `pub async fn insert_attached_corpus(&self, content_hash: &str, origin: &str, title_hint: Option<&str>, metadata: &serde_json::Value, status: CorpusStatus, stage: Stage, attachment: &NewFile<'_>) -> Result<Insertion>`
  - `pub async fn set_read_text(&self, id: &str, text: &str, shingles: Vec<u64>) -> Result<()>`
  - `pub async fn clear_read_text(&self, id: &str) -> Result<()>`

- [ ] **Step 1: Rename `NewImage` to `NewFile`**

```bash
grep -rln 'NewImage' src/ | xargs sed -i 's/\bNewImage\b/NewFile/g'
```

Then widen its doc comment: it is no longer "one image" but "the bytes a corpus was captured from, when it was not text" — which is what the module doc already says.

- [ ] **Step 2: Rename the read-text writers**

```bash
grep -rln 'set_described_text\|clear_described_text' src/ \
  | xargs sed -i -e 's/\bset_described_text\b/set_read_text/g' \
                 -e 's/\bclear_described_text\b/clear_read_text/g'
```

Update their doc comments in `src/store/corpora.rs` so they no longer say "the vision stage": these are what *a* reading stage wrote — vision for a photo, extraction for a PDF — and the status is still the caller's to set.

- [ ] **Step 3: Generalize the insert**

In `src/store/corpora.rs`, rename `insert_image_corpus` to `insert_attached_corpus` and take the status and the stage instead of hardcoding them:

```rust
    /// A corpus whose source is bytes rather than text: the row, its
    /// attachment and the unit that will read it land in one transaction. A
    /// capture with no attachment, or one no job will ever read, is a row that
    /// lies about what it holds.
    ///
    /// `status` and `stage` are the caller's because they are what differs
    /// between the doors: a photo starts `describing` and waits for `Describe`,
    /// a PDF starts `extracting` and waits for `Extract`.
    pub async fn insert_attached_corpus(
        &self,
        content_hash: &str,
        origin: &str,
        title_hint: Option<&str>,
        metadata: &serde_json::Value,
        status: CorpusStatus,
        stage: Stage,
        attachment: &super::attachments::NewFile<'_>,
    ) -> Result<Insertion> {
```

Inside: `status: CorpusStatus::Describing` becomes `status`, `enqueue_with(&mut *tx, Stage::Describe, ...)` becomes `enqueue_with(&mut *tx, stage, ...)`, and the conflict message loses the word "image":

```rust
                Error::Store("a file capture conflicted with a corpus that then vanished".into())
```

- [ ] **Step 4: Fix the call sites**

`src/core/ingest.rs` (in `ingest_image`) and the four test call sites in `src/store/corpora.rs`, `src/jobs/describe.rs`, `src/web/corpus_view.rs`, `src/web/ui.rs` each gain the same two arguments, between `metadata` and the attachment:

```rust
                CorpusStatus::Describing,
                Stage::Describe,
```

Every existing call site is an image capture, so all five get exactly those two values and none of their behaviour changes. `Stage` and `CorpusStatus` may need importing at the test call sites.

- [ ] **Step 5: Run the whole suite**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: green, with no test edited beyond the renames. If a test's *assertion* had to change, the refactor changed behaviour — revert and find out why.

- [ ] **Step 6: Commit**

```bash
git add -A src/
git commit -m "refactor: a stored capture is a file with a reading stage, not necessarily a photo"
```

---

## Task 3: The status, the stage, the origin, and the door into the core

**Files:**
- Modify: `src/store/corpora.rs` (`CorpusStatus::Extracting`)
- Modify: `src/store/jobs.rs` (`Stage::Extract`)
- Modify: `src/core/ingest.rs` (`ORIGIN_PDF`, `PdfCapture`, `ingest_pdf`)
- Modify: `src/config.rs`, `config.example.toml` (`pdf_max_bytes`)

**Interfaces:**
- Consumes: `core::pdf::to_markdown` (Task 1, not called here); `Store::insert_attached_corpus`, `NewFile` (Task 2).
- Produces:
  - `pub const ORIGIN_PDF: &str = "pdf";`
  - `pub struct PdfCapture { pub bytes: Vec<u8>, pub filename: Option<String>, pub title_hint: Option<String>, pub note: Option<String> }`
  - `pub async fn ingest_pdf(&self, c: PdfCapture) -> Result<IngestOutcome>` on `Core`
  - `CapturingConfig.pdf_max_bytes: usize`

- [ ] **Step 1: Write the failing tests**

In `src/core/ingest.rs`'s test module:

```rust
    fn a_pdf() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    #[tokio::test]
    async fn a_pdf_is_stored_whole_and_queued_to_be_extracted() {
        let core = test_core().await;
        let out = core
            .ingest_pdf(PdfCapture {
                bytes: a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: Some("the quarterly plan".into()),
            })
            .await
            .unwrap();

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Extracting);
        assert_eq!(src.origin, ORIGIN_PDF);
        assert_eq!(src.raw_text, "", "the text arrives from the stage, not here");
        assert_eq!(src.metadata["note"], "the quarterly plan");
        assert_eq!(src.metadata["file"]["name"], "plan.pdf");

        let (mime, bytes) = core
            .store
            .attachment_original(&out.id)
            .await
            .unwrap()
            .expect("the PDF itself is kept");
        assert_eq!(mime, "application/pdf");
        assert_eq!(bytes, a_pdf(), "stored byte for byte");

        let job = core.store.claim_job().await.unwrap().expect("a job");
        assert_eq!(job.stage, Stage::Extract);
        assert_eq!(job.target_id, out.id);
    }

    #[tokio::test]
    async fn the_same_pdf_twice_is_one_corpus() {
        let core = test_core().await;
        let first = core.ingest_pdf(a_capture()).await.unwrap();
        let again = core.ingest_pdf(a_capture()).await.unwrap();
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);
    }

    fn a_capture() -> PdfCapture {
        PdfCapture {
            bytes: a_pdf(),
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: None,
        }
    }
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test --lib core::ingest::tests::a_pdf_is_stored
```

Expected: does not compile — `PdfCapture`, `ingest_pdf`, `CorpusStatus::Extracting` and `Stage::Extract` do not exist.

- [ ] **Step 3: Add the status**

In `src/store/corpora.rs`, beside `Describing`:

```rust
    /// A PDF whose text has not been extracted yet. Only file corpora hold it.
    Extracting,
```

and `"extracting"` in both `as_str` and `parse`. Check `web::ui::status_badge` for an exhaustive `match` on `CorpusStatus` — if it is exhaustive, `Extracting` takes the same badge as `Describing`; both mean "in flight, not readable yet".

- [ ] **Step 4: Add the stage**

In `src/store/jobs.rs`:

```rust
    /// One captured PDF, read into the markdown that becomes its `raw_text`,
    /// then handed off to `Synthesize`. Local work: no inference call, so no
    /// role gates it and no budget is spent.
    Extract,
```

plus `"extract"` in `as_str` and `parse`.

- [ ] **Step 5: Add the origin and the capture**

In `src/core/ingest.rs`, beside `ORIGIN_IMAGE`:

```rust
/// A PDF, whichever door it arrived through. Its own value for the same reason
/// a photo has one: the queue and the detail page have to tell a document that
/// was extracted from one that was typed.
pub const ORIGIN_PDF: &str = "pdf";
```

and beside `ImageCapture`:

```rust
/// One PDF, whichever door it arrived through.
#[derive(Debug, Clone)]
pub struct PdfCapture {
    pub bytes: Vec<u8>,
    pub filename: Option<String>,
    pub title_hint: Option<String>,
    pub note: Option<String>,
}
```

- [ ] **Step 6: Implement `ingest_pdf`**

Beside `ingest_image`. Note what it deliberately does *not* do: no `describer` check (extraction needs no role, so the PDF door is open on a bare config), no decode permit (there is no pixel work), and no preview.

```rust
    /// Store the bytes and queue the reading. No gate: extraction is local, so
    /// unlike the image door this one is open whatever `[infer]` holds.
    pub async fn ingest_pdf(&self, c: PdfCapture) -> Result<IngestOutcome> {
        let PdfCapture {
            bytes,
            filename,
            title_hint,
            note,
        } = c;
        // Hashed before anything else touches it: the same PDF sent twice
        // costs one SHA-256 the second time, not an extraction.
        let hash = content_hash(&bytes);
        if let Some(existing) = self.store.find_by_hash(&hash).await? {
            tracing::info!(corpus_id = %existing.id, "duplicate PDF, returning existing source");
            return Ok(IngestOutcome::existing(&existing));
        }

        let mut metadata = serde_json::json!({
            "file": {
                "name": filename.clone(),
                "bytes": bytes.len(),
                "mime": "application/pdf",
            },
        });
        if let Some(n) = clean_note(note) {
            metadata["note"] = serde_json::json!(n);
        }

        let inserted = self
            .store
            .insert_attached_corpus(
                &hash,
                ORIGIN_PDF,
                title_hint.as_deref(),
                &metadata,
                CorpusStatus::Extracting,
                Stage::Extract,
                &crate::store::attachments::NewFile {
                    kind: "pdf",
                    mime: "application/pdf",
                    filename: filename.as_deref(),
                    // No preview: rendering a first page needs pdfium, which
                    // is the ML rung's dependency and not this one's.
                    preview: &[],
                    bytes: &bytes,
                    width: None,
                    height: None,
                },
            )
            .await?;
        match inserted {
            Insertion::Existing(c) => Ok(IngestOutcome::existing(&c)),
            Insertion::Created(c) => Ok(IngestOutcome {
                id: c.id,
                status: c.status,
                duplicate: false,
                near_duplicate: None,
            }),
        }
    }
```

Check `ingest_image` for how it builds its `file` metadata (`super::image::file_facts`) and match the key names it uses, so the corpus page's `metadata_rows` finds them without a second code path.

- [ ] **Step 7: Add the config key**

`src/config.rs`, in `CaptureConfig`:

```rust
    /// Bytes an uploaded PDF may weigh. A book scan is tens of megabytes; this
    /// is the per-route ceiling for the upload door, the global body limit
    /// stays. Nothing else bounds a PDF — no page cap: feeding a book to
    /// engram is a deliberate act, and the queue is already throttled.
    pub pdf_max_bytes: usize,
```

with `pdf_max_bytes: 50 * 1024 * 1024` in `Default`. Mirror the wording into `config.example.toml` beside `image_max_bytes`:

```toml
# Bytes an uploaded PDF may weigh. The upload door has its own ceiling for
# PDFs, above the 8 MB request-body limit; a book is tens of megabytes.
pdf_max_bytes = 52428800
```

There is a test asserting `config.example.toml` mentions `image_max_bytes` (`src/config.rs`, search `contains("image_max_bytes")`). Add the same assertion for `pdf_max_bytes` next to it, and a `assert_eq!(c.capture.pdf_max_bytes, 50 * 1024 * 1024)` beside the existing `image_max_bytes` default assertion.

- [ ] **Step 8: Run**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: green.

- [ ] **Step 9: Commit**

```bash
git add -A src/ config.example.toml
git commit -m "feat: a captured PDF is kept whole and waits for a stage to read it"
```

---

## Task 4: The extraction stage

**Files:**
- Create: `src/jobs/extract.rs`
- Modify: `src/jobs/mod.rs` (module, dispatch, exhaustion, permanent failure)
- Modify: `src/core/ingest.rs` (`reprocess` gains a `Stage::Extract` arm)

**Interfaces:**
- Consumes: `core::pdf::to_markdown` (Task 1); `Store::attachment_original`, `set_read_text`, `clear_read_text` (Task 2); `Stage::Extract`, `CorpusStatus::Extracting`, `ORIGIN_PDF` (Task 3).
- Produces: `pub async fn run(core: &Core, corpus_id: &str) -> Result<()>` and `pub async fn park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()>` in `crate::jobs::extract`.

- [ ] **Step 1: Write the failing tests**

In a `#[cfg(test)] mod tests` at the bottom of `src/jobs/extract.rs`:

```rust
    use super::*;
    use crate::core::ingest::PdfCapture;
    use crate::core::test_support::test_core;
    use crate::store::jobs::Stage;

    fn a_pdf() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    async fn captured(core: &Core, bytes: Vec<u8>) -> String {
        core.ingest_pdf(PdfCapture {
            bytes,
            filename: Some("plan.pdf".into()),
            title_hint: None,
            note: None,
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn extraction_writes_the_markdown_and_hands_off_to_synthesize() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        core.store.claim_job().await.unwrap(); // the Extract job
        run(&core, &id).await.unwrap();

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert!(src.raw_text.contains("quarterly plan lists three goals"));
        assert!(!src.shingles.is_empty(), "comparable to other captures");

        let next = core.store.claim_job().await.unwrap().expect("synthesize queued");
        assert_eq!(next.stage, Stage::Synthesize);
        assert_eq!(next.target_id, id);
    }

    #[tokio::test]
    async fn the_whole_pipeline_takes_a_pdf_to_ready() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        while crate::jobs::run_one(&core).await.unwrap() {}
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready, "{:?}", src.status);
        assert!(!core.store.artifacts_for_corpus(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pdf_that_cannot_be_read_is_parked_as_failed_with_the_reason() {
        let core = test_core().await;
        // Stored through the store rather than the door, because the door
        // does not read the bytes and so cannot refuse these.
        let id = captured(&core, b"%PDF-1.4 and then garbage".to_vec()).await;
        assert!(crate::jobs::run_one(&core).await.unwrap());

        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Failed);
        assert!(
            src.metadata["extract"]["error"].as_str().unwrap().contains("PDF"),
            "{:?}",
            src.metadata
        );
        assert!(
            !core.store.live_job(Stage::Extract, &id).await.unwrap(),
            "the job is closed, not re-armed: these bytes will not improve"
        );
        assert!(
            src.near_dupe_of.is_none(),
            "not a near-duplicate; not on the review queue"
        );
    }

    #[tokio::test]
    async fn a_re_extraction_replaces_the_reading_and_everything_from_it() {
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        while crate::jobs::run_one(&core).await.unwrap() {}

        core.reprocess(&id, Stage::Extract).await.unwrap();
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Extracting);
        assert_eq!(src.raw_text, "", "the old reading is gone, not merged");
        assert!(core.store.artifacts_for_corpus(&id).await.unwrap().is_empty());

        while crate::jobs::run_one(&core).await.unwrap() {}
        let src = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready);
        assert!(src.raw_text.contains("quarterly plan"));
    }

    #[tokio::test]
    async fn only_a_captured_file_can_be_re_extracted() {
        let core = test_core().await;
        let out = core.ingest("just some pasted text", "web", None).await.unwrap();
        assert!(matches!(
            core.reprocess(&out.id, Stage::Extract).await,
            Err(crate::error::Error::Validation(_))
        ));
    }

    #[tokio::test]
    async fn parking_survives_a_metadata_column_that_is_not_an_object() {
        // `meta["extract"] = ...` panics on anything but an object or null,
        // and the value comes out of a column. A worker is the wrong place to
        // find that out. Same hazard as `jobs::describe::park_failed`.
        let core = test_core().await;
        let id = captured(&core, a_pdf()).await;
        core.store
            .set_corpus_metadata(&id, &serde_json::json!("not an object"))
            .await
            .unwrap();

        park_failed(&core, &id, "gpu on fire").await.unwrap();

        let got = core.store.get_corpus(&id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Failed);
        assert_eq!(got.metadata["extract"]["error"].as_str(), Some("gpu on fire"));
    }
```

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test --lib jobs::extract
```

Expected: does not compile — the module does not exist.

- [ ] **Step 3: Write the stage**

`src/jobs/extract.rs`, built after `src/jobs/describe.rs` — read that file first and keep the shapes identical where the logic is:

```rust
//! The extraction stage: one captured PDF, no model call, and a corpus that
//! from here on is text like any other.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::corpora::CorpusStatus;

pub async fn run(core: &Core, corpus_id: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    if src.status != CorpusStatus::Extracting {
        tracing::info!(
            corpus_id,
            status = src.status.as_str(),
            "already extracted; nothing to do"
        );
        return Ok(());
    }
    let Some((_, bytes)) = core.store.attachment_original(corpus_id).await? else {
        return Err(Error::Store(format!(
            "pdf corpus {corpus_id} has no attachment"
        )));
    };

    // `to_markdown` walks up to `pdf_max_bytes` synchronously. Held on a Tokio
    // worker that is seconds during which search, health and the queue poll on
    // that thread all wait; see `web::api::extract` for the same move.
    let text = tokio::task::spawn_blocking(move || crate::core::pdf::to_markdown(&bytes))
        .await
        .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?;

    let text = match text {
        Ok(t) => t,
        // A PDF that cannot be parsed will not parse better on the fourth
        // attempt. Park it now with the reason on its page.
        Err(e @ Error::Validation(_)) => {
            park_failed(core, corpus_id, &e.to_string()).await?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // Before the text is written: the scan reads every stored signature, and
    // this row's is still empty, so it cannot match itself.
    let sig = crate::store::shingle::signature(&text);
    let near = core
        .store
        .find_near_duplicate(&sig, core.consolidate.near_dupe_min)
        .await?;
    core.store.set_read_text(corpus_id, &text, sig).await?;
    core.park_or_queue(corpus_id, near.as_ref()).await?;
    tracing::info!(
        corpus_id,
        chars = text.len(),
        parked = near.is_some(),
        "pdf extracted"
    );
    Ok(())
}

/// The extraction is not going to happen — the bytes are not a document this
/// build can read. The file stays; the corpus says why it stopped, on its page
/// and on Ops, and `reprocess(Extract)` is the way back once the build changes.
pub async fn park_failed(core: &Core, corpus_id: &str, reason: &str) -> Result<()> {
    let src = core.store.get_corpus(corpus_id).await?;
    let mut meta = src.metadata.clone();
    // Indexing a `Value` that is not an object panics, and this one comes
    // straight out of a column. The reason is the whole point of the write, so
    // a column holding something else is started over rather than left to take
    // the worker down.
    if !meta.is_object() {
        meta = serde_json::json!({});
    }
    meta["extract"] = serde_json::json!({ "error": reason });
    core.store.set_corpus_metadata(corpus_id, &meta).await?;
    core.store
        .set_corpus_status(corpus_id, CorpusStatus::Failed)
        .await?;
    tracing::warn!(corpus_id, reason, "pdf could not be read; parked as failed");
    Ok(())
}
```

- [ ] **Step 4: Wire it into the queue**

`src/jobs/mod.rs`:

1. `pub mod extract;` beside `pub mod embed;`
2. In the dispatch `match`, beside the `Describe` arm:

```rust
        (Stage::Extract, _) => extract::run(core, &job.target_id).await,
```

3. In the exhausted-retries `match`, beside the `Describe` arm — and note it carries **no role guard**, unlike `Describe`'s `core.describer.is_some()`: extraction needs nothing configured, so an exhausted extraction is a real failure and never a wait.

```rust
                // The PDF is stored, so nothing is lost by stopping — but a
                // corpus shown as in flight forever is a lie. No role guard:
                // extraction needs nothing configured, so this cannot be a
                // wait for a role that has not arrived.
                (Stage::Extract, _) if exhausted => {
                    tracing::warn!(error = %e, "could not extract this PDF; parking it");
                    extract::park_failed(core, &job.target_id, &e.to_string()).await?;
                    core.store.complete_job(job.id).await?;
                }
```

4. In the permanent-failure `match`, beside the `Describe` arm:

```rust
                (Stage::Extract, _) => {
                    extract::park_failed(core, &job.target_id, &e.to_string()).await?;
                    core.store.complete_job(job.id).await?;
                }
```

Read `park_failed_if_still_there` in that file — if it takes a stage or is describe-specific, either generalize it to take the parking function or call `extract::park_failed` directly, whichever leaves the image path untouched.

`needs_model` needs no change: `Extract` falls through to `None`, which is correct — it calls no model.

- [ ] **Step 5: Add the reprocess arm**

In `src/core/ingest.rs`, in `reprocess`, beside the `Stage::Describe` arm:

```rust
            // A stored PDF can always be read again — with the ML build, or
            // after a docling upgrade. The reading and everything derived from
            // it are replaced wholesale, because an artifact of the old
            // reading has no span in the new one.
            Stage::Extract => {
                if !self.store.has_attachment(&src.id).await? || src.origin != ORIGIN_PDF {
                    return Err(Error::Validation(
                        "only a captured PDF can be re-extracted".into(),
                    ));
                }
                self.forget_derived_work(&src.id).await?;
                self.store.clear_read_text(&src.id).await?;
                let mut meta = src.metadata.clone();
                if let Some(m) = meta.as_object_mut() {
                    m.remove("extract");
                }
                self.store.set_corpus_metadata(&src.id, &meta).await?;
                self.store
                    .set_corpus_status(&src.id, CorpusStatus::Extracting)
                    .await?;
                self.heal_dangling_supersessions().await?;
                self.store
                    .enqueue(Stage::Extract, "corpus", &src.id)
                    .await?;
            }
```

Also widen the guard at the top of the `Synthesize | Enrich` arm. It currently refuses to re-segment an unread image; an unextracted PDF is the same situation and the message must name it:

```rust
                if src.status == CorpusStatus::Describing
                    || (src.origin == ORIGIN_IMAGE && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this image has not been read yet — re-read it instead".into(),
                    ));
                }
                if src.status == CorpusStatus::Extracting
                    || (src.origin == ORIGIN_PDF && src.raw_text.trim().is_empty())
                {
                    return Err(Error::Validation(
                        "this PDF has not been extracted yet — re-extract it instead".into(),
                    ));
                }
```

- [ ] **Step 6: Run**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: green, including every image test unchanged.

- [ ] **Step 7: Commit**

```bash
git add -A src/
git commit -m "feat: a stored PDF is read into markdown by a stage that can be run again"
```

---

## Task 5: The upload door

**Files:**
- Modify: `src/web/api.rs` (`named_pdf`, `upload`, `api_router`, `get_file`)
- Modify: `src/web/mod.rs` (pass `pdf_max_bytes` into `api_router`)

**Interfaces:**
- Consumes: `Core::ingest_pdf`, `PdfCapture` (Task 3); `capture.pdf_max_bytes` (Task 3).
- Produces: `pub fn api_router(image_max_bytes: usize, pdf_max_bytes: usize) -> Router<AppState>`; route `GET /api/v1/corpora/{id}/file`.

- [ ] **Step 1: Write the failing tests**

In `src/web/api.rs`'s test module, following the shapes already there (`multipart`, `FilePart`, `json_of`):

```rust
    fn a_pdf() -> Vec<u8> {
        include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec()
    }

    #[tokio::test]
    async fn the_upload_door_takes_a_pdf_and_answers_accepted() {
        let (app, token) = app_and_token().await;
        let body = multipart(
            "b1",
            &[FilePart {
                field: "file",
                filename: "plan.pdf",
                mime: "application/pdf",
                bytes: a_pdf(),
            }],
            &[],
        );
        let res = app
            .oneshot(
                Request::post("/api/v1/corpora/upload")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "multipart/form-data; boundary=b1")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // 202, not 201: stored, but the reading that makes it searchable is
        // still queued — the same promise the image door makes.
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        let j = json_of(res).await;
        assert_eq!(j["status"], "extracting");
    }

    /// A part may legally carry no `Content-Type`; the name is then the only
    /// thing the sender told us, exactly as for `.txt`.
    #[tokio::test]
    async fn a_pdf_named_pdf_but_declaring_nothing_is_still_taken() {
        let (app, token) = app_and_token().await;
        let body = multipart(
            "b2",
            &[FilePart {
                field: "file",
                filename: "plan.pdf",
                mime: "",
                bytes: a_pdf(),
            }],
            &[],
        );
        let res = app
            .oneshot(
                Request::post("/api/v1/corpora/upload")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "multipart/form-data; boundary=b2")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    /// The door widened by one format, not to anything binary.
    #[tokio::test]
    async fn a_zip_is_refused_by_type_and_by_name() {
        let (app, token) = app_and_token().await;
        for (mime, name) in [("application/zip", "a.zip"), ("", "a.zip")] {
            let body = multipart(
                "b3",
                &[FilePart {
                    field: "file",
                    filename: name,
                    mime,
                    bytes: b"PK\x03\x04".to_vec(),
                }],
                &[],
            );
            let res = app
                .clone()
                .oneshot(
                    Request::post("/api/v1/corpora/upload")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "multipart/form-data; boundary=b3")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "a zip declared `{mime}` named `{name}` was let in"
            );
        }
    }

    /// After `the_image_door_has_its_own_larger_body_limit`: a PDF over the
    /// global 8 MB must reach the handler rather than be cut off by the router.
    #[tokio::test]
    async fn the_upload_door_has_its_own_larger_body_limit() {
        let (app, token) = app_and_token().await;
        // Bigger than `web::MAX_BODY_BYTES`, smaller than `pdf_max_bytes`.
        let mut bytes = a_pdf();
        bytes.resize(9 * 1024 * 1024, b' ');
        let body = multipart(
            "b4",
            &[FilePart {
                field: "file",
                filename: "big.pdf",
                mime: "application/pdf",
                bytes,
            }],
            &[],
        );
        let res = app
            .oneshot(
                Request::post("/api/v1/corpora/upload")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "multipart/form-data; boundary=b4")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Whatever the verdict, it is the handler's and not the router's:
        // 413 would mean the global limit is still in force here.
        assert_ne!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_original_pdf_comes_back_from_the_file_route() {
        let (app, token, core) = app_token_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: a_pdf(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap()
            .id;
        let res = app
            .oneshot(
                Request::get(format!("/api/v1/corpora/{id}/file"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()["content-type"],
            "application/pdf",
            "the route answers with the attachment's own type"
        );
        let got = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(got.to_vec(), a_pdf(), "byte for byte as uploaded");
    }
```

`multipart` and `FilePart` come from `crate::web::test_support`; check `FilePart`'s field names against that module before writing — the shape above follows its current definition, and if a field is spelled differently there, that spelling wins.

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test --lib web::api
```

- [ ] **Step 3: Widen the door**

In `src/web/api.rs`, beside `named_txt`:

```rust
/// Whether an upload's filename claims to be a PDF. Consulted on the same
/// terms as `named_txt`: only when the part carried no `Content-Type`.
fn named_pdf(filename: Option<&str>) -> bool {
    filename.is_some_and(|n| {
        std::path::Path::new(n)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    })
}
```

Then restructure `upload`'s type check into three outcomes rather than a pass/fail, keeping the existing reasoning about an absent `Content-Type` intact:

```rust
    // A part may legally carry no `Content-Type` at all, and letting that skip
    // the check turns the accepted list into "anything whose bytes happen to
    // parse" — a `.csv`, a `.json`, a page of HTML. An absent type is not a
    // pass; it just moves the question to the name.
    enum Kind {
        Text,
        Pdf,
    }
    let kind = if declared.is_empty() {
        if named_txt(filename.as_deref()) {
            Kind::Text
        } else if named_pdf(filename.as_deref()) {
            Kind::Pdf
        } else {
            return Err(Error::Validation(
                "that upload declares no type and is named neither `.txt` nor \
                 `.pdf` — only text/plain and application/pdf are accepted"
                    .into(),
            ));
        }
    } else if declared.starts_with("text/plain") {
        Kind::Text
    } else if declared.starts_with("application/pdf") {
        Kind::Pdf
    } else {
        return Err(Error::Validation(format!(
            "that file is `{declared}` — only text/plain and application/pdf are accepted"
        )));
    };
```

The `Text` branch keeps the existing UTF-8 conversion and `ingest_capture` call verbatim, including its `201`/`200`. The `Pdf` branch:

```rust
        Kind::Pdf => {
            let out = st
                .core
                .ingest_pdf(crate::core::ingest::PdfCapture {
                    bytes: bytes.to_vec(),
                    filename,
                    title_hint: None,
                    note,
                })
                .await?;
            // 202, not 201: stored, but the extraction that makes it a corpus
            // anyone can search is still queued.
            let code = if out.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            Ok((code, Json(out)))
        }
```

- [ ] **Step 4: Give the route its ceiling and add the file route**

```rust
pub fn api_router(image_max_bytes: usize, pdf_max_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/corpora", post(ingest).get(list_corpora))
        // Its own ceiling: a PDF is many times the global limit, and this door
        // now takes one.
        .route(
            "/corpora/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(pdf_max_bytes)),
        )
```

and beside the image route:

```rust
        .route("/corpora/{id}/file", get(get_file))
```

with a handler that is `get_image`'s original branch and nothing else:

```rust
/// The bytes as uploaded, whatever they are. The image door's `?original=1`
/// answers the same thing for a photo and stays where it is; this is the name
/// that does not lie about a PDF.
async fn get_file(
    State(st): State<AppState>,
    _id: Identity,
    Path(id): Path<String>,
) -> Result<axum::response::Response> {
    use axum::response::IntoResponse;
    let Some((mime, bytes)) = st.core.store.attachment_original(&id).await? else {
        return Err(Error::NotFound);
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=3600".to_string(),
            ),
        ],
        bytes,
    )
        .into_response())
}
```

Update the one caller in `src/web/mod.rs`:

```rust
            api::api_router(
                state.core.capture.image_max_bytes,
                state.core.capture.pdf_max_bytes,
            ),
```

- [ ] **Step 5: Run**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

- [ ] **Step 6: Commit**

```bash
git add -A src/
git commit -m "feat: the upload door takes a PDF, and the original comes back by its own name"
```

---

## Task 6: The screens

**Files:**
- Modify: `src/web/corpus_view.rs` (the third label case, module doc)
- Modify: `src/web/ui.rs` (`pdf` on the corpus template, `unread`, the error row)
- Modify: `src/web/templates/corpus.html` (Re-extract button, the original link)
- Modify: `src/web/templates/capture.html` (`accept`, the wording)

**Interfaces:**
- Consumes: `ORIGIN_PDF`, `CorpusStatus::Extracting` (Task 3); `Stage::Extract` via `reprocess_ui` (Task 4); `GET /corpora/{id}/file` (Task 5).
- Produces: `CorpusTemplate.pdf: bool`.

- [ ] **Step 1: Write the failing tests**

In `src/web/corpus_view.rs`'s test module, beside the transcription test:

```rust
    #[tokio::test]
    async fn a_pdf_corpus_labels_its_lines_as_an_extraction() {
        // The lines belong to docling, not to the PDF's layout, and a span
        // into them is a claim about what was extracted. Same move as the
        // image arm's `transcription`, same reason.
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("h", crate::core::ingest::ORIGIN_PDF, None)
            .await
            .unwrap();
        s.set_read_text(&src.id, "a\nb\nc", vec![]).await.unwrap();
        let src = s.get_corpus(&src.id).await.unwrap();
        let out = slice(&src, None, 0);
        assert_eq!(out.label, "extraction");
    }
```

And in `src/web/ui.rs`'s test module:

```rust
    #[tokio::test]
    async fn a_pdf_corpus_page_offers_re_extract_and_names_the_failure() {
        let (app, cookie, core) = app_cookie_and_core().await;
        let id = core
            .ingest_pdf(crate::core::ingest::PdfCapture {
                bytes: include_bytes!("../../tests/fixtures/one-heading.pdf").to_vec(),
                filename: Some("plan.pdf".into()),
                title_hint: None,
                note: None,
            })
            .await
            .unwrap()
            .id;
        crate::jobs::extract::park_failed(&core, &id, "that PDF holds no extractable text")
            .await
            .unwrap();

        let page = get_page(&app, &cookie, &format!("/ui/corpora/{id}")).await;
        assert!(
            page.contains("no extractable text"),
            "the reason is what the page is for: {page}"
        );
        assert!(
            page.contains(r#"value="extract""#),
            "no Re-extract on a PDF that failed: {page}"
        );
        assert!(
            page.contains("/file"),
            "the original is not reachable: {page}"
        );
        assert!(
            !page.contains("Re-segment"),
            "nothing was extracted; there is nothing to re-segment: {page}"
        );
    }
```

`app_cookie_and_core` and `get_page` are the names this file's existing corpus-page tests use — check them and use whatever those tests actually call, rather than introducing helpers. Search `src/web/ui.rs` for `set_read_text`: there is already a test that builds an image corpus and renders its page, and this one is its twin.

- [ ] **Step 2: Run and watch them fail**

```bash
cargo test --lib web::corpus_view web::ui
```

- [ ] **Step 3: The third label**

In `src/web/corpus_view.rs`, replace the `transcript` bool with a label chosen once:

```rust
    // An image corpus's lines are the model's reading of the picture, and a
    // PDF's are docling's extraction of it. The label says so in both cases: a
    // span into either is a claim about what was written down, not about what
    // the source showed.
    let headless_label = match source.origin.as_str() {
        crate::core::ingest::ORIGIN_IMAGE => "transcription",
        crate::core::ingest::ORIGIN_PDF => "extraction",
        _ => "corpus",
    };
```

and use it where `transcript` was consulted. Then fix the module doc, which currently promises the wrong thing:

```rust
//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A text source is answered by its lines; an image source by the model's
//! reading of the picture, labelled as such; a PDF by docling's extraction of
//! it, labelled as such. All three count lines: a page number would be a nicer
//! label and a second coordinate system, and the spec rejected it.
```

- [ ] **Step 4: The corpus page**

`src/web/ui.rs`, beside `image`:

```rust
    /// A PDF corpus: the lines below are docling's extraction of it rather
    /// than the document itself, and the original is one click away.
    pdf: bool,
```

built beside it, with `unread` widened to cover both:

```rust
    let image = s.origin == crate::core::ingest::ORIGIN_IMAGE;
    let pdf = s.origin == crate::core::ingest::ORIGIN_PDF;
    let unread = (image && (s.status == CorpusStatus::Describing || s.raw_text.trim().is_empty()))
        || (pdf && (s.status == CorpusStatus::Extracting || s.raw_text.trim().is_empty()));
```

and `pdf` added to the `CorpusTemplate { .. }` construction.

In `metadata_rows`, beside the `describe` row:

```rust
    if let Some(e) = m["extract"]["error"].as_str() {
        rows.push(("Extraction".into(), e.into()));
    }
```

Update `reprocess_ui`'s doc comment, which currently enumerates the stages:

```rust
/// Re-segment by default; `stage=describe` re-reads a captured image and
/// `stage=extract` re-reads a captured PDF.
```

No code change there — `Stage::parse` already accepts `"extract"` from Task 3.

- [ ] **Step 5: The corpus template**

`src/web/templates/corpus.html`, after the image block:

```html
  {% if pdf %}
  <a class="btn btn-sm" href="/api/v1/corpora/{{ id }}/file">Original PDF</a>
  <form method="post" action="/ui/corpora/{{ id }}/reprocess" style="display:inline"
        onsubmit="return confirm('Read the PDF again? The current extraction and its artifacts are replaced.')">
    <input type="hidden" name="stage" value="extract">
    <button class="btn btn-sm" type="submit">Re-extract</button>
  </form>
  {% endif %}
```

The comment above the Re-segment guard mentions only images; widen it to say a PDF with no extraction yet has nothing to re-segment either.

- [ ] **Step 6: The capture page**

`src/web/templates/capture.html`. Both `accept` attributes gain PDF, and the wording follows:

```html
    <input type="file" name="file" accept=".txt,text/plain,.pdf,application/pdf,image/*" hidden>
    <span>…or drop a <code>.txt</code>, a PDF or an image here — on a phone, tap to take a photo.</span>
```

and, in the no-vision branch:

```html
    <input type="file" name="file" accept=".txt,text/plain,.pdf,application/pdf" hidden>
    <span>…or drop a <code>.txt</code> or a PDF here.</span>
```

The JavaScript needs **one** change. `send()` routes by MIME into `image` or `file`, and a PDF correctly falls to `file` — but the success line then says "Captured." while the extraction is still queued, which is the same lie the image branch already avoids:

```js
      var isImage = file.type.indexOf('image/') === 0;
      var isPdf = file.type === 'application/pdf' || /\.pdf$/i.test(file.name || '');
```

and in the result line:

```js
          result.textContent = pair[0]
            ? (isImage ? 'Captured — the photo is queued to be read.'
               : isPdf ? 'Captured — the PDF is queued to be extracted.'
               : 'Captured.')
            : (pair[1].error || 'Upload failed.');
```

The default filename `'paste.txt'` must not be applied to a PDF that arrived without a name, or the door will refuse it by name:

```js
      var fallbackName = isImage ? 'photo.jpg' : (isPdf ? 'capture.pdf' : 'paste.txt');
      payload.append(isImage ? 'image' : 'file', file, file.name || fallbackName);
```

- [ ] **Step 7: Run**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

- [ ] **Step 8: See it work**

Start the app, drop a real PDF on the capture page, and follow it to `ready`. A test that renders a template is not a test that the drop zone posts.

- [ ] **Step 9: Commit**

```bash
git add -A src/
git commit -m "feat: a PDF is a door on the capture page and an extraction on its own"
```

---

## Task 7: Retract `page 42`, and say the door exists

Two files promise a label this plan did not build, and two describe the doors without this one.

**Files:**
- Modify: `ROADMAP.md`
- Modify: `README.md`
- Verify: `src/web/corpus_view.rs` module doc (already fixed in Task 6, Step 3)
- Verify: `src/web/api.rs` — `upload`'s doc comment still says "`.txt` and nothing else, for now"

- [ ] **Step 1: Fix the `upload` doc comment**

If Task 5 left it as it was, replace it:

```rust
/// `.txt` and PDF, and nothing else. Refusing everything else by name is what
/// keeps this one from quietly ingesting the bytes of a format it cannot read.
/// A PDF is stored and queued; the extraction happens in `Stage::Extract`.
```

- [ ] **Step 2: Rewrite the ROADMAP bullet**

Replace the **PDF capture** bullet under *Core Platform & Tooling* — it is now built, and it did not deliver the page label it promised:

```markdown
- **A CLI.** PDF capture is built: `docling` reads an uploaded PDF into markdown
  in `Stage::Extract`, and the corpus is text like any other from there. Spans
  into it are line spans labelled `extraction`, not `page 42` — a page map is a
  second coordinate system beside every stored span, and the label is not worth
  it. The ML rung (layout and table models, ONNX) is behind `--features pdf-ml`
  and off by default; a scanned PDF has no text layer and is refused with that
  reason until it is switched on.
```

Also add a line for the formats now within reach, since the crate is in the tree:

```markdown
- **DOCX, EPUB, XLSX and the rest.** `docling` already reads them; only a door
  and a `kind` are missing. Deliberately not part of PDF capture.
```

- [ ] **Step 3: Say so in the README**

In the "Corpus, segment, artifact" section, after the image paragraph, a paragraph in the same voice:

```markdown
A corpus can also be a **PDF** — dropped on the capture page or sent to
`POST /api/v1/corpora/upload` (multipart `file`, optional `note`). The file is
kept untouched and served at `GET /api/v1/corpora/{id}/file`; `docling` reads
it into markdown in the background, locally and without a model, and from there
it is a text corpus like any other. The lines shown beside an artifact are that
extraction rather than the page as it was laid out, and the pane says so.
A PDF that holds no extractable text — a scan — is shown as `failed` with the
reason; **Re-extract** on that page (or
`POST /api/v1/corpora/{id}/reprocess` with `{"stage":"extract"}`) reads it again
from the stored original, with whatever build is running now.
```

- [ ] **Step 4: Check nothing else still promises pages**

```bash
grep -rn "page 42" . --include='*.rs' --include='*.md' --include='*.html'
```

Expected: no hits outside the spec's own "rejected" discussion.

- [ ] **Step 5: Run the suite once more and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add -A
git commit -m "docs: the PDF door is built, and it counts lines rather than pages"
```

---

## Self-review notes

Checked against the spec, section by section:

| spec section | task |
|---|---|
| What this is not | no task — background |
| The engine (dependency, `pdf-ml`, pinning, comment) | Task 1, Step 2 |
| Provenance: lines labelled as extracted | Task 6, Step 3 |
| the two `page 42` retractions | Task 7, Steps 2–3, and Task 6 Step 3 for the module doc |
| Storage, no schema change | Task 3, Step 6 (`NewFile`, `kind: "pdf"`, empty preview) |
| Ingest, `ORIGIN_PDF`, hash first, no gate, `spawn_blocking` | Task 3 Steps 5–6; the `spawn_blocking` in Task 4 Step 3 |
| The job, hand-off, park, reprocess | Task 4 |
| Size, `pdf_max_bytes`, no page cap | Task 3 Step 7, Task 5 Step 4 |
| Web: `named_pdf`, body limit, `/file`, capture page, `slice()` | Tasks 5 and 6 |
| Markdown rendering: nothing to do | no task, by design |
| Testing: fixture, body limit, `named_pdf`, garbage-bytes park, label | Tasks 1, 4, 5, 6 |
| Out of scope | no task, by design |
| The spike | Task 0 |
