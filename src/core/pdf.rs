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
/// replacement box. A list set in Arial or Times has the same shape with a
/// real U+2022, which markdown does not read as a marker either. Both arrive
/// *detached*, on a line of their own, because the marker and the item are
/// separate text runs at different x positions. Folding the two together is
/// what turns them back into a list.
///
/// And the export separates every text block by a blank line. That matters
/// more than it looks: `pdf-text` recovers no headings (see
/// `the_text_rung_recovers_no_headings`), so blank lines are exactly what
/// `infer::split` falls back to for window boundaries — a list whose items sit
/// in their own blank-line-separated blocks is cut into one window per item.
///
/// Two things this deliberately does *not* do, because both would destroy
/// text rather than repair it:
///
/// Private-use codepoints are only removed where they lead a line and stand
/// apart from it, which is the one position where they are standing in for a
/// marker. Elsewhere they are left exactly as they are: Big5 and HKSCS encode
/// thousands of real hanzi in U+E000–U+F848, and a subsetting producer can map
/// ligatures and ordinary letters into the same block, so a filter that ran
/// over the whole line would silently eat words. An unrenderable box in the
/// corpus is a visible defect; a deleted character is not.
///
/// And indentation is kept. With `--features pdf-ml` the export is structured
/// markdown — nested lists, indented continuations, indented code — where
/// leading whitespace carries the nesting. Only trailing whitespace goes.
fn normalise(md: String) -> String {
    let mut out: Vec<String> = Vec::new();
    // A marker on a line of its own belongs to the next line that has text.
    let mut pending_marker = false;
    // Blank lines since the last line — text or detached marker — that this
    // consumed. It decides both how far a marker reaches and whether two items
    // still belong to one list.
    let mut gap = 0usize;
    // Whether the last line written was a list item, so that the single blank
    // line between two items can be dropped and the list stay one block.
    let mut last_was_item = false;

    for line in md.lines() {
        let line = line.trim_end();
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let (text, had_marker) = strip_marker(body);

        if text.is_empty() {
            if had_marker {
                // A line that held nothing but the marker is the detached one.
                pending_marker = true;
                gap = 0;
            } else {
                gap += 1;
                // The export puts exactly one blank line between blocks, so
                // more than one is a break the document itself drew. Neither a
                // marker nor a list carries across it — otherwise an ornament
                // closing one section bullets the next, and two lists with a
                // break between them are glued into one.
                if gap > 1 {
                    pending_marker = false;
                    last_was_item = false;
                }
            }
            continue;
        }

        // A pending marker never overwrites a line that is already something:
        // on the `pdf-ml` rung the next line can be a heading, and `- ## Two`
        // is a heading `infer::split` no longer recognises as a boundary.
        let item = had_marker || (pending_marker && !is_markdown_structure(text));

        // One blank line, never a run — and none at all between two items of
        // the same list.
        if gap > 0 && !out.is_empty() && !(item && last_was_item) {
            out.push(String::new());
        }
        out.push(if item {
            format!("{indent}- {text}")
        } else {
            format!("{indent}{text}")
        });
        last_was_item = item;
        pending_marker = false;
        gap = 0;
    }

    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Split a leading marker off `body`, which has no surrounding whitespace.
///
/// A marker is one glyph, and what follows it is a space or nothing at all.
/// Word and LibreOffice always emit it as its own text run, so it is always
/// set off that way and it is never a run of glyphs — which is what a Big5
/// word opening with private-use hanzi looks like. Such a word is part of the
/// text and is left where it is.
fn strip_marker(body: &str) -> (&str, bool) {
    let mut chars = body.chars();
    if !chars.next().is_some_and(is_marker) {
        return (body, false);
    }
    let rest = chars.as_str();
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        (rest.trim_start(), true)
    } else {
        (body, false)
    }
}

fn is_marker(c: char) -> bool {
    is_private_use(c) || is_bullet_glyph(c)
}

/// The three private-use ranges: the BMP block and the two supplementary
/// planes. Symbol-font bullets land in the first, but a document can carry any
/// of them and none of them render.
fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

/// Bullets that render perfectly well and are still not markdown markers — a
/// list set in an ordinary font arrives as these. The lowercase `o` Word uses
/// at the second level is deliberately absent: a line starting `o` is prose far
/// more often than it is a list, and reading it as a marker would eat a word.
fn is_bullet_glyph(c: char) -> bool {
    matches!(
        c,
        '\u{2022}' // •
            | '\u{2023}' // ‣
            | '\u{2043}' // ⁃
            | '\u{2219}' // ∙
            | '\u{00B7}' // ·
            | '\u{25AA}' // ▪
            | '\u{25AB}' // ▫
            | '\u{25A0}' // ■
            | '\u{25CB}' // ○
            | '\u{25CF}' // ●
            | '\u{25E6}' // ◦
    )
}

/// Whether a line already carries markdown structure of its own. `text` has had
/// its indentation removed.
fn is_markdown_structure(text: &str) -> bool {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    text.starts_with('#')
        || text.starts_with('|')
        || text.starts_with("```")
        || text.starts_with("~~~")
        || text.starts_with("> ")
        || matches!(text.split_once(' '), Some(("-" | "*" | "+", _)))
        || (digits > 0 && text[digits..].starts_with(". "))
}

#[cfg(test)]
mod tests {
    /// The reachability argument in `.cargo/audit.toml` rests on this file.
    ///
    /// Two `quick-xml` advisories are ignored there — RUSTSEC-2026-0194 and
    /// -0195 — because the vulnerable copy belongs to `calamine`, which docling
    /// reaches only from its spreadsheet backends, and this application asks
    /// docling for exactly one format. `from_path` would pick the format from a
    /// file extension and could therefore select one of those backends; the
    /// declared `InputFormat::Pdf` cannot.
    ///
    /// If this fails, do not adjust it: the two entries in `.cargo/audit.toml`
    /// have stopped being true and the advisories are live.
    #[test]
    fn docling_is_only_ever_asked_for_a_pdf() {
        let src = include_str!("pdf.rs");
        let body = &src[..src.find("mod tests").expect("the test module")];
        assert_eq!(
            body.matches("InputFormat::").count(),
            1,
            "one input format is named, and it decides which docling backends exist for us"
        );
        assert!(body.contains("docling::InputFormat::Pdf"), "and it is Pdf");
        assert!(
            !body.contains("from_path"),
            "`from_path` detects the format from the extension, which can select \
             a spreadsheet backend — see `.cargo/audit.toml`"
        );
    }

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

    /// A glyph inside a sentence is not a marker and is not removed either.
    /// Deleting it would be deleting text: the same block holds real hanzi in
    /// a Big5 document and subset-mapped letters in a pdfTeX one, and neither
    /// is distinguishable from an ornament here. A box in the corpus is a
    /// visible defect; a missing character is a silent one.
    #[test]
    fn a_glyph_mid_sentence_is_left_where_it_is() {
        assert_eq!(
            normalise("cost \u{f0b7} benefit\n".into()),
            "cost \u{f0b7} benefit\n"
        );
    }

    /// The marker is always its own text run, so it is always set off by a
    /// space. A private-use codepoint that opens a word is part of the word —
    /// this is what a Big5 line looks like, and it must survive whole.
    #[test]
    fn a_glyph_that_opens_a_word_is_not_a_marker() {
        assert_eq!(
            normalise("\u{e6b0}\u{e6b1}\u{e6b2} and more\n".into()),
            "\u{e6b0}\u{e6b1}\u{e6b2} and more\n"
        );
    }

    /// A list in an ordinary font arrives with a real U+2022, detached the
    /// same way. Markdown does not read that as a marker any more than it
    /// reads U+F0B7 as one.
    #[test]
    fn a_real_bullet_is_a_marker_too() {
        let md = normalise("\u{2022} first\n\n\u{25aa}\n\nsecond\n".into());
        assert_eq!(md, "- first\n- second\n", "{md:?}");
    }

    /// Word's second-level marker is a lowercase `o`, and a line of prose can
    /// start with one. Reading it as a marker would eat a word, so it is not
    /// one.
    #[test]
    fn a_lowercase_o_is_not_a_marker() {
        assert_eq!(
            normalise("o shaped like a ring\n".into()),
            "o shaped like a ring\n"
        );
    }

    /// An ornament closing a section is a glyph on a line of its own, exactly
    /// like a detached marker. It must not bullet the next heading: `- ## Two`
    /// is not a heading to `infer::split`, so the boundary would be lost.
    #[test]
    fn an_orphan_glyph_does_not_bullet_a_heading() {
        let md = normalise("one\n\n\u{f0b7}\n\n## Two\n\nunder it\n".into());
        assert_eq!(md, "one\n\n## Two\n\nunder it\n", "{md:?}");
    }

    /// Nor does it reach across a break the document itself drew.
    #[test]
    fn an_orphan_glyph_does_not_reach_across_a_block_break() {
        let md = normalise("\u{f0b7}\n\n\n\na new paragraph\n".into());
        assert_eq!(md, "a new paragraph\n", "{md:?}");
    }

    /// Dropping the blank line between two items keeps one list together; it
    /// must not weld two lists into one when the document separated them.
    #[test]
    fn two_lists_with_a_break_between_them_stay_apart() {
        let md = normalise("\u{f0b7} one\n\n\n\n\u{f0b7} two\n".into());
        assert_eq!(md, "- one\n\n- two\n", "{md:?}");
    }

    /// With `--features pdf-ml` the export is structured markdown and leading
    /// whitespace is the nesting. Trimming it flattens a two-level list and
    /// unindents a code block, which changes what the corpus says.
    #[test]
    fn indentation_survives_because_it_carries_structure() {
        let md = normalise("- one\n  - nested\n\n    code line  \n".into());
        assert_eq!(md, "- one\n  - nested\n\n    code line\n", "{md:?}");
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
