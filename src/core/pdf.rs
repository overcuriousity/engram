//! PDF → markdown, and the only place `docling` is named.
//! CPU-bound without yielding, so every caller runs it under `spawn_blocking`.

use crate::error::{Error, Result};

/// Two paths reach it: docling refuses a page with no text layer, and a whitespace-only layer
/// exports to nothing.
const NO_TEXT_LAYER: &str = "that PDF holds no extractable text — it is probably a scan. \
     Reading one needs a build with `--features pdf-ml`, which adds the OCR models.";

pub fn to_markdown(bytes: &[u8]) -> Result<String> {
    let source = docling::SourceDocument::from_bytes(
        // docling's label for the input; the real filename is on the capture's metadata.
        "capture.pdf",
        docling::InputFormat::Pdf,
        bytes.to_vec(),
    );
    let converted = docling::DocumentConverter::new()
        .convert(source)
        .map_err(|e| {
            let detail = e.to_string();
            // docling names its own cargo feature here, not one an engram operator would set.
            if detail.contains("no embedded text layer") {
                return Error::Validation(NO_TEXT_LAYER.into());
            }
            Error::Validation(format!("that PDF could not be read: {detail}"))
        })?;

    // `PartialSuccess` still holds most pages worth keeping; `Failure` holds nothing usable.
    if converted.status == docling::ConversionStatus::Failure {
        return Err(Error::Validation(
            "that PDF could not be read: the parser reported failure".into(),
        ));
    }

    let md = normalise(converted.document.export_to_markdown());
    if md.trim().is_empty() {
        return Err(Error::Validation(NO_TEXT_LAYER.into()));
    }
    Ok(md)
}

/// Folds detached list markers (U+F0B7, U+2022) onto their items and drops blank lines between
/// items, so `infer::split` does not cut a window per item. Private-use codepoints go only where
/// they stand alone as a marker: Big5/HKSCS put real hanzi there.
fn normalise(md: String) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut pending_marker = false;
    // Blank lines consumed since the last line; decides how far a marker reaches and whether a list
    // continues.
    let mut gap = 0usize;
    let mut last_was_item = false;

    for line in md.lines() {
        let line = line.trim_end();
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let (text, had_marker) = strip_marker(body);

        if text.is_empty() {
            if had_marker {
                pending_marker = true;
                gap = 0;
            } else {
                gap += 1;
                // The export puts exactly one blank line between blocks, so more is a break the
                // document drew.
                if gap > 1 {
                    pending_marker = false;
                    last_was_item = false;
                }
            }
            continue;
        }

        // On the `pdf-ml` rung the next line can be a heading, and `- ## Two` is no longer a
        // boundary.
        let item = had_marker || (pending_marker && !is_markdown_structure(text));

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

/// A marker is one glyph followed by a space or nothing; a run of private-use glyphs is a Big5
/// word.
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

/// The BMP block and the two supplementary planes; none of them render.
fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

/// Word's second-level `o` is absent: a line starting `o` is prose far more often than a list.
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

/// Whether a line already carries markdown structure; `text` has had its indentation removed.
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

    // Generated with `soffice --headless --convert-to pdf --outdir <dir> <a two-line .html>`;
    // regenerate, do not edit.
    const ONE_HEADING: &[u8] = include_bytes!("../../tests/fixtures/one-heading.pdf");
    const NO_TEXT: &[u8] = include_bytes!("../../tests/fixtures/no-text.pdf");
    const BULLET_LIST: &[u8] = include_bytes!("../../tests/fixtures/bullet-list.pdf");
    const LIGATURE: &[u8] = include_bytes!("../../tests/fixtures/ligature.pdf");
    const IDENTITY_H: &[u8] = include_bytes!("../../tests/fixtures/identity-h.pdf");

    #[test]
    fn a_pdf_becomes_markdown_carrying_its_words() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            md.contains("quarterly plan lists three goals"),
            "the body did not survive extraction: {md}"
        );
    }

    #[test]
    fn the_text_rung_recovers_no_headings() {
        let md = to_markdown(ONE_HEADING).unwrap();
        assert!(
            !md.lines().any(|l| l.trim_start().starts_with('#')),
            "structure is being recovered now — see this test's doc comment: {md}"
        );
    }

    #[test]
    fn a_ligature_comes_out_as_its_letters() {
        let md = to_markdown(LIGATURE).unwrap();
        for word in ["Begrifflichkeiten", "Definition", "Begriff"] {
            assert!(
                md.contains(word),
                "`{word}` did not come out verbatim: {md:?}"
            );
        }
        assert!(
            !md.chars()
                .any(|c| c == '\u{0600}' || ('\u{FB00}'..='\u{FB06}').contains(&c)),
            "a ligature survived as a codepoint of its own: {md:?}"
        );
    }

    #[test]
    fn two_byte_text_comes_out_as_whole_words() {
        let md = to_markdown(IDENTITY_H).unwrap();
        for line in [
            "Du hast eine Datei an Ronny gesendet.",
            "Forensik und deren Begrifflichkeiten",
            "Jede Spur wird gesichert, bevor sie gedeutet wird.",
        ] {
            assert!(md.contains(line), "`{line}` did not come out whole: {md:?}");
        }
        assert!(!md.contains('\0'), "a NUL reached the text: {md:?}");
    }

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

    #[test]
    fn a_page_of_nothing_but_glyphs_is_not_a_corpus() {
        assert_eq!(normalise("\u{f0b7}\n\n\u{e000}\n".into()), "\n");
    }

    #[test]
    fn a_glyph_mid_sentence_is_left_where_it_is() {
        assert_eq!(
            normalise("cost \u{f0b7} benefit\n".into()),
            "cost \u{f0b7} benefit\n"
        );
    }

    #[test]
    fn a_glyph_that_opens_a_word_is_not_a_marker() {
        assert_eq!(
            normalise("\u{e6b0}\u{e6b1}\u{e6b2} and more\n".into()),
            "\u{e6b0}\u{e6b1}\u{e6b2} and more\n"
        );
    }

    #[test]
    fn a_real_bullet_is_a_marker_too() {
        let md = normalise("\u{2022} first\n\n\u{25aa}\n\nsecond\n".into());
        assert_eq!(md, "- first\n- second\n", "{md:?}");
    }

    #[test]
    fn a_lowercase_o_is_not_a_marker() {
        assert_eq!(
            normalise("o shaped like a ring\n".into()),
            "o shaped like a ring\n"
        );
    }

    #[test]
    fn an_orphan_glyph_does_not_bullet_a_heading() {
        let md = normalise("one\n\n\u{f0b7}\n\n## Two\n\nunder it\n".into());
        assert_eq!(md, "one\n\n## Two\n\nunder it\n", "{md:?}");
    }

    #[test]
    fn an_orphan_glyph_does_not_reach_across_a_block_break() {
        let md = normalise("\u{f0b7}\n\n\n\na new paragraph\n".into());
        assert_eq!(md, "a new paragraph\n", "{md:?}");
    }

    #[test]
    fn two_lists_with_a_break_between_them_stay_apart() {
        let md = normalise("\u{f0b7} one\n\n\n\n\u{f0b7} two\n".into());
        assert_eq!(md, "- one\n\n- two\n", "{md:?}");
    }

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
