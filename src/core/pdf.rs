//! PDF → markdown, and the only place `docling` is named.
//!
//! Synchronous and CPU-bound: it walks the whole document without yielding, so
//! every caller runs it under `spawn_blocking`. See `web::api::extract` for the
//! same reasoning about `dom_smoothie`.
//!
//! The default build is docling's `pdf-text` rung — a pure-Rust text parser
//! with no models behind it. `--features pdf-ml` puts the layout and table
//! models there instead; nothing in this file changes, which is the point.

use crate::error::{Error, Result};

pub fn to_markdown(bytes: &[u8]) -> Result<String> {
    let source = docling::SourceDocument::from_bytes(
        // The name is docling's label for the input and never reaches a
        // corpus: the real filename is recorded on the capture's metadata by
        // the door, which is the only place it means anything.
        "capture.pdf",
        docling::InputFormat::Pdf,
        bytes.to_vec(),
    );
    let converted = docling::DocumentConverter::new()
        .convert(source)
        .map_err(|e| Error::Validation(format!("that PDF could not be read: {e}")))?;

    // `PartialSuccess` is kept rather than refused: a document whose last page
    // defeated the parser is still worth most of what it holds, and refusing
    // it would throw away the pages that did come out. `Failure` is not — it
    // means the document produced nothing anyone can use.
    if converted.status == docling::ConversionStatus::Failure {
        return Err(Error::Validation(
            "that PDF could not be read: the parser reported failure".into(),
        ));
    }

    let md = converted.document.export_to_markdown();
    if md.trim().is_empty() {
        // A PDF of scanned pages has no text layer, and `pdf-text` cannot
        // invent one. Saying so beats a corpus that is silently empty and a
        // synthesis failure three stages downstream.
        return Err(Error::Validation(
            "that PDF holds no extractable text — it is probably a scan, \
             which needs the pdf-ml build"
                .into(),
        ));
    }
    Ok(md)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both fixtures were generated, not written by hand:
    //   soffice --headless --convert-to pdf --outdir <dir> <a two-line .html>
    // Regenerate them that way rather than editing bytes.
    const ONE_HEADING: &[u8] = include_bytes!("../../tests/fixtures/one-heading.pdf");
    /// A page holding a filled rectangle and no text at all — what a scanned
    /// PDF looks like to a parser with no OCR behind it.
    const NO_TEXT: &[u8] = include_bytes!("../../tests/fixtures/no-text.pdf");

    #[test]
    fn a_pdf_becomes_markdown_carrying_its_words() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            md.contains("quarterly plan lists three goals"),
            "the body did not survive extraction: {md}"
        );
    }

    /// Pins a known limitation so that fixing it cannot pass unnoticed.
    ///
    /// The `pdf-text` rung recovers no document structure: measured on two
    /// real documents it produced complete text in correct reading order and
    /// zero headings. Heading detection is the layout model, which
    /// `--features pdf-ml` gates, so this is a property of the build and not
    /// something to fix here.
    ///
    /// It matters because `infer::split` prefers headings as window
    /// boundaries and carries the last one into the next window. Without them
    /// it falls back to blank lines and every window loses its section
    /// context.
    ///
    /// **If this test fails, that is good news**: docling started recovering
    /// structure. Delete it, and tell the splitter's boundary comment.
    #[test]
    fn the_text_rung_recovers_no_headings() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            !md.lines().any(|l| l.trim_start().starts_with('#')),
            "structure is being recovered now — see this test's doc comment: {md}"
        );
    }

    #[test]
    fn bytes_that_are_not_a_pdf_are_an_error_naming_the_input() {
        let e = to_markdown(b"this is not a pdf at all").unwrap_err();
        let msg = e.to_string();
        assert!(msg.to_lowercase().contains("pdf"), "unhelpful error: {msg}");
        assert!(
            !e.retryable(),
            "these bytes will not parse better on the fourth attempt"
        );
    }

    #[test]
    fn a_pdf_with_no_text_layer_is_an_error_rather_than_an_empty_corpus() {
        // A page that extracts to "" must not become a corpus with no text:
        // synthesis would then run on nothing and the failure would surface
        // three stages away from its cause. This is what a scan looks like to
        // the `pdf-text` rung.
        // docling refuses this itself, before an empty export can happen, and
        // its message already names both the cause and the way out. The guard
        // in `to_markdown` stays for a text layer that holds only whitespace,
        // which docling does not treat as absent.
        let e = to_markdown(NO_TEXT).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("no embedded text layer"),
            "unhelpful error: {msg}"
        );
        assert!(!e.retryable(), "a scan does not become readable on a retry");
    }
}
