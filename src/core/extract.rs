use crate::error::{Error, Result};

/// Rewrite setext headings as ATX ones.
///
/// `html2md` renders `<h1>` and `<h2>` underlined — `Title` over `-----` —
/// which is valid markdown and invisible to `src/infer/split.rs`, whose
/// `is_heading` matches a leading `#` and nothing else. Left alone, every
/// captured page would hand the splitter a document with no boundaries in it
/// at exactly the two levels an article uses most, which is the loss this
/// module exists to prevent. Cheaper and far more predictable than teaching
/// the splitter a second heading syntax it would then have to carry through
/// windowing and line numbering.
///
/// Fenced code is left exactly as it arrived. Inside a fence a `---` under a
/// non-blank line is content — YAML front matter, a config snippet, a rule
/// drawn in ASCII — and rewriting it would both invent a heading the document
/// never had and *delete* the underline line, which is a stored artifact
/// quietly differing from the page it was captured from.
fn setext_to_atx(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut fence: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // Fence state first, and nothing else happens on a fence line.
        match fence {
            Some(open) => {
                // Only the marker that opened it closes it, so a ``` block
                // quoting ~~~ does not end early.
                if fence_marker(line) == Some(open) {
                    fence = None;
                }
                out.push(line.to_string());
                i += 1;
                continue;
            }
            None => {
                if let Some(m) = fence_marker(line) {
                    fence = Some(m);
                    out.push(line.to_string());
                    i += 1;
                    continue;
                }
            }
        }

        let next = lines.get(i + 1).copied().unwrap_or("");
        let underline = next.trim_end();
        // An underline only makes a heading of a non-blank line above it. A
        // `---` under a blank line is a thematic break, and turning that into
        // a heading would invent a section that is not in the document.
        let level = if line.trim().is_empty() || underline.len() < 2 {
            None
        } else if underline.chars().all(|c| c == '=') {
            Some("#")
        } else if underline.chars().all(|c| c == '-') {
            Some("##")
        } else {
            None
        };
        match level {
            Some(hashes) => {
                out.push(format!("{hashes} {}", line.trim()));
                i += 2;
            }
            None => {
                out.push(line.to_string());
                i += 1;
            }
        }
    }
    out.join("\n")
}

/// The fence marker a line opens or closes a code block with, if any.
///
/// Up to three leading spaces, per CommonMark; a fourth makes it indented code
/// instead — which needs no tracking here, because an indented `---` carries
/// its indent into the all-dashes test below and fails it.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m))
}

/// Turn a rendered page into the markdown the segmenter wants.
///
/// Markdown rather than plain text on purpose. `src/infer/split.rs` splits a
/// corpus on headings first and a token budget second, so extraction that
/// flattens `<h2>` into an undistinguished line costs the segmenter its
/// primary boundary and every artifact downstream is drawn from a worse slice.
/// The structure the page already had is structure the splitter can use.
///
/// `base_url` resolves relative links, so a captured document's references
/// still point somewhere a year later.
///
/// Synchronous, and it must stay that way. `Readability` holds a
/// `dom_query::Document`, which is `!Send`; alive across an `.await` it would
/// make the enclosing future `!Send` and axum would refuse the handler. Every
/// non-`Send` value here is created and dropped inside this call.
pub fn html_to_markdown(
    html: &str,
    base_url: Option<&url::Url>,
    min_chars: usize,
) -> Result<String> {
    let content = {
        let mut readability =
            dom_smoothie::Readability::new(html, base_url.map(url::Url::as_str), None)
                .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
        let article = readability
            .parse()
            .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
        article.content.to_string()
    };

    let markdown = setext_to_atx(&html2md::parse_html(&content))
        .trim()
        .to_string();

    // Nothing at all is its own case, and it holds even where the floor does
    // not. A selection is exempt from the floor because the operator picked
    // the text and three sentences are a legitimate capture — but a fragment
    // readability found no content in is an empty corpus, and storing one
    // silently is worse than saying so.
    if markdown.is_empty() {
        return Err(Error::Validation(
            "nothing survived extraction — there was no text in what was captured".into(),
        ));
    }

    // The guard the whole path exists for. A server-side GET does not fail
    // loudly when it is served a login wall — it succeeds, and returns the
    // wall. Counting what survived extraction is how that becomes an error
    // instead of a corpus nobody can tell apart from a real one.
    let extracted = markdown.chars().count();
    if extracted < min_chars {
        return Err(Error::Validation(format!(
            "only {extracted} characters extracted, below the {min_chars} the capture needs — \
             the page was probably a login wall or an empty shell"
        )));
    }
    Ok(markdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = r#"
        <html><head><title>Mounting an image</title></head><body>
          <nav><a href="/">home</a><a href="/about">about</a></nav>
          <article>
            <h1>Mounting an image</h1>
            <p>The loop device is what makes this work at all, and the
               paragraph has to be long enough that readability scores it as
               content rather than as furniture, so here is some more of it.</p>
            <h2>Read-only first</h2>
            <p>Always mount read-only until you have a hash of the source
               image, because a mount that replays a dirty journal writes to
               the evidence you were trying to preserve.</p>
            <p><a href="/notes/hashing">Hashing notes</a></p>
          </article>
          <footer>© nobody</footer>
        </body></html>
    "#;

    #[test]
    fn extraction_keeps_headings_the_splitter_needs() {
        // `src/infer/split.rs` prefers a heading boundary over a blank line and
        // carries the last heading into the next window. Extraction that
        // flattened <h2> would cost every artifact downstream a worse slice.
        let md = html_to_markdown(ARTICLE, None, 10).unwrap();
        assert!(
            md.contains("## Read-only first"),
            "the h2 must survive as a markdown heading, got:\n{md}"
        );
        assert!(crate::infer::split::is_heading_for_test(
            "## Read-only first"
        ));
    }

    #[test]
    fn extraction_drops_the_furniture() {
        let md = html_to_markdown(ARTICLE, None, 10).unwrap();
        assert!(!md.contains("© nobody"), "footer survived:\n{md}");
        assert!(!md.contains("about"), "navigation survived:\n{md}");
    }

    #[test]
    fn a_relative_link_is_resolved_against_the_page_it_came_from() {
        let base = url::Url::parse("https://example.test/notes/mounting").unwrap();
        let md = html_to_markdown(ARTICLE, Some(&base), 10).unwrap();
        assert!(
            md.contains("https://example.test/notes/hashing"),
            "a captured document's references must still point somewhere, got:\n{md}"
        );
    }

    #[test]
    fn a_page_that_reduces_to_boilerplate_is_refused_not_captured() {
        // A login wall. It extracts to almost nothing, and the caller is told
        // so rather than handed a corpus made of the subscribe prompt.
        let wall = "<html><body><div id=\"root\"></div>\
                    <p>Subscribe to read.</p></body></html>";
        let err = html_to_markdown(wall, None, 200).unwrap_err();
        assert!(
            matches!(err, Error::Validation(ref m) if m.contains("extracted")),
            "expected a validation error naming extraction, got {err:?}"
        );
    }

    #[test]
    fn a_thematic_break_is_not_mistaken_for_a_heading() {
        // `---` under a blank line separates sections; it does not name one.
        // Promoting it would invent a heading the document does not have.
        let out = setext_to_atx("intro\n\n---\n\nrest");
        assert_eq!(out, "intro\n\n---\n\nrest");
    }

    #[test]
    fn both_setext_levels_become_the_hashes_the_splitter_reads() {
        assert_eq!(setext_to_atx("Title\n====="), "# Title");
        assert_eq!(setext_to_atx("Section\n-----"), "## Section");
    }

    #[test]
    fn a_rule_inside_a_code_block_is_left_exactly_as_it_arrived() {
        // A captured page full of config snippets is the normal case, and a
        // `---` in one is content: YAML front matter, a table rule, a divider
        // drawn by hand. Promoting it invents a heading the document never had
        // *and deletes the line*, so the stored artifact quietly differs from
        // the page it came from.
        let fenced = "before\n\n```yaml\nfoo\n---\nbar\n```\n\nafter";
        assert_eq!(setext_to_atx(fenced), fenced);

        let tilde = "~~~\ntitle\n===\n~~~";
        assert_eq!(setext_to_atx(tilde), tilde);

        // A ``` block quoting ~~~ is not closed by the quote.
        let nested = "```\n~~~\nfoo\n---\n```\n\nSection\n---";
        assert_eq!(
            setext_to_atx(nested),
            "```\n~~~\nfoo\n---\n```\n\n## Section"
        );
    }

    #[test]
    fn a_heading_after_a_code_block_is_still_promoted() {
        // The fence tracking must not swallow the rest of the document: what
        // the splitter needs is the headings, and a page with one snippet in
        // it still has to hand them over.
        let md = "```\ncode\n```\n\nRead-only first\n---\n\nbody";
        assert_eq!(
            setext_to_atx(md),
            "```\ncode\n```\n\n## Read-only first\n\nbody"
        );
    }

    #[test]
    fn html_that_is_not_a_document_at_all_is_an_error_not_a_panic() {
        let err = html_to_markdown("", None, 200).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}
