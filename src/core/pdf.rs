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

/// Said in one place because two paths reach it: docling refuses a page with
/// no text layer outright, and a text layer holding only whitespace exports to
/// nothing without any error at all.
const NO_TEXT_LAYER: &str = "that PDF holds no extractable text — it is probably a scan. \
     Reading one needs a build with `--features pdf-ml`, which adds the OCR models.";

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
        .map_err(|e| {
            let detail = e.to_string();
            // docling names its own cargo feature here, and it is not the one an
            // engram operator would set. Answer in this application's terms or the
            // corpus page sends them looking for a flag that does not exist.
            if detail.contains("no embedded text layer") {
                return Error::Validation(NO_TEXT_LAYER.into());
            }
            Error::Validation(format!("that PDF could not be read: {detail}"))
        })?;

    // `PartialSuccess` is kept rather than refused: a document whose last page
    // defeated the parser is still worth most of what it holds, and refusing
    // it would throw away the pages that did come out. `Failure` is not — it
    // means the document produced nothing anyone can use.
    if converted.status == docling::ConversionStatus::Failure {
        return Err(Error::Validation(
            "that PDF could not be read: the parser reported failure".into(),
        ));
    }

    let md = normalise(converted.document.export_to_markdown());
    if md.trim().is_empty() {
        // A PDF of scanned pages has no text layer, and `pdf-text` cannot
        // invent one. Saying so beats a corpus that is silently empty and a
        // synthesis failure three stages downstream.
        return Err(Error::Validation(NO_TEXT_LAYER.into()));
    }
    Ok(md)
}

/// What `export_to_markdown` hands back is not clean text yet, and the two
/// things wrong with it both reach the splitter.
///
/// A bulleted list written in Word or LibreOffice draws its bullet from a
/// symbol font, and the PDF's own `ToUnicode` map points that glyph at a
/// private-use codepoint — U+F0B7 for Symbol's bullet. It is a real character
/// in the text layer, so extraction keeps it, and no font on the reading side
/// maps it: it survives into the corpus, the passages and the paste as a
/// replacement box. It also arrives *detached*, on a line of its own, because
/// the marker and the item are separate text runs at different x positions.
/// Folding the two together is what turns them back into a list.
///
/// And the export separates every text block by more than one blank line.
/// That matters more than it looks: `pdf-text` recovers no headings (see
/// `the_text_rung_recovers_no_headings`), so blank lines are exactly what
/// `infer::split` falls back to for window boundaries — a list whose items sit
/// in their own blank-line-separated blocks is cut into one window per item.
///
/// Nothing here removes a word. Private-use codepoints render as nothing
/// anywhere, so they are not text that could be lost.
fn normalise(md: String) -> String {
    let mut out: Vec<String> = Vec::new();
    // A bullet glyph on its own line belongs to the next line that has text.
    let mut orphan_bullet = false;
    // Whether the last line written was a list item, so that the blank line
    // between two items can be dropped and the list stay one block.
    let mut last_was_item = false;

    for line in md.lines() {
        // Only a glyph the line *starts* with is standing in for a marker; one
        // in the middle of a sentence is the same unrenderable character but
        // not a list, so it is dropped without turning the line into an item.
        let had_bullet = line.trim_start().starts_with(is_private_use);
        let stripped: String = line.chars().filter(|c| !is_private_use(*c)).collect();
        let text = stripped.trim();

        if text.is_empty() {
            // A line that held nothing but the glyph is the detached marker.
            if had_bullet {
                orphan_bullet = true;
            } else if out.last().is_some_and(|l| !l.is_empty()) {
                // One blank line, never a run — and none at all between two
                // items of the same list.
                out.push(String::new());
            }
            continue;
        }

        if had_bullet || orphan_bullet {
            if last_was_item && out.last().is_some_and(|l| l.is_empty()) {
                out.pop();
            }
            out.push(format!("- {text}"));
            last_was_item = true;
        } else {
            out.push(text.to_string());
            last_was_item = false;
        }
        orphan_bullet = false;
    }

    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The three private-use ranges: the BMP block and the two supplementary
/// planes. Symbol-font bullets land in the first, but a document can carry any
/// of them and none of them render.
fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
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
    /// A bulleted list whose markers are a symbol font's bullet, mapped by the
    /// PDF's own `ToUnicode` to U+F0B7 — what Word and LibreOffice emit, and
    /// what the reading side cannot render. Built by
    /// `tests/fixtures/bullet-list.py`; regenerate it with that, not by hand.
    const BULLET_LIST: &[u8] = include_bytes!("../../tests/fixtures/bullet-list.pdf");

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

    /// The fixture's raw export carries `\u{f0b7}` on lines of its own and
    /// blank-line runs between every block; neither may reach a corpus.
    #[test]
    fn a_symbol_font_bullet_becomes_a_list_marker() {
        let md = to_markdown(BULLET_LIST).unwrap();
        assert!(
            !md.chars().any(is_private_use),
            "an unrenderable glyph survived into the corpus: {md:?}"
        );
        assert!(
            md.contains("- ship the extraction door"),
            "the detached marker did not fold into its item: {md:?}"
        );
    }

    #[test]
    fn runs_of_blank_lines_are_collapsed() {
        let md = to_markdown(BULLET_LIST).unwrap();
        assert!(
            !md.contains("\n\n\n"),
            "a blank-line run reached the splitter, which boundaries on it: {md:?}"
        );
        assert!(
            md.contains("three goals.\n\nEach of them"),
            "the paragraph break itself has to survive: {md:?}"
        );
    }

    /// A list is one thing to read, and the splitter's fallback boundary is a
    /// blank line — so items separated by one become a window each.
    #[test]
    fn a_list_stays_one_block() {
        let md = normalise("\u{f0b7}\n\nfirst\n\n\u{f0b7}\n\nsecond\n\nafter\n".into());
        assert_eq!(md, "- first\n- second\n\nafter\n", "{md:?}");
    }

    #[test]
    fn a_marker_that_leads_its_own_item_is_folded_in_place() {
        let md = normalise("\u{f0b7} first\n\u{f0b7} second\n".into());
        assert_eq!(md, "- first\n- second\n", "{md:?}");
    }

    /// Normalisation runs before the empty check, so a page holding nothing
    /// but unrenderable glyphs answers with the scan message rather than
    /// becoming a corpus of markers.
    #[test]
    fn a_page_of_nothing_but_glyphs_is_not_a_corpus() {
        assert_eq!(normalise("\u{f0b7}\n\n\u{e000}\n".into()), "\n");
    }

    /// A glyph inside a sentence is the same unrenderable character, but the
    /// line is prose and must not become a list item.
    #[test]
    fn a_glyph_mid_sentence_is_dropped_without_making_a_list() {
        assert_eq!(
            normalise("cost \u{f0b7} benefit\n".into()),
            "cost  benefit\n"
        );
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(
            normalise("one line\n\nanother line\n".into()),
            "one line\n\nanother line\n"
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
        // docling refuses this before an empty export can happen; the guard in
        // `to_markdown` stays for a text layer holding only whitespace, which
        // docling does not treat as absent. Both answer in the same words.
        let e = to_markdown(NO_TEXT).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("no extractable text"),
            "unhelpful error: {msg}"
        );
        assert!(
            msg.contains("pdf-ml"),
            "the way out has to be named in this application's terms, not \
             docling's cargo features: {msg}"
        );
        assert!(!e.retryable(), "a scan does not become readable on a retry");
    }
}
