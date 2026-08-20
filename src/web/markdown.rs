use pulldown_cmark::{Event, Options, Parser, html};

fn options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_FOOTNOTES);
    o
}

/// Render chunk markdown to HTML, then sanitize.
///
/// Chunk text is written by a language model and displayed inside the
/// operator's authenticated session, so it is untrusted input by definition.
/// Sanitizing after rendering catches both raw HTML passed through by the
/// markdown parser and anything the parser itself emits.
pub fn render(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, options());
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);

    ammonia::Builder::default()
        .link_rel(Some("noopener noreferrer nofollow"))
        .add_generic_attributes(["class"])
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .clean(&unsafe_html)
        .to_string()
}

/// Render text that is not markdown: a passage kept as the document wrote it.
///
/// Escaped rather than parsed, and inside a `<pre>`, because the line breaks
/// are the structure. Read as markdown, a table of contents lifted out of a
/// PDF loses every break and becomes one paragraph of stretched leader dots,
/// and a section number written `# 3` becomes a heading.
///
/// Escaping is the whole sanitization: no markup is produced from the input,
/// so there is nothing to clean afterwards. Three replacements are the
/// complete set for text content — quotes and spaces only need escaping inside
/// an attribute, and `ammonia::clean_text` escaping them too turned every
/// space and newline of a document into a numeric entity.
pub fn render_verbatim(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<pre class=\"verbatim\">{escaped}</pre>")
}

/// Plain-text preview with markup removed. Used in list views where rendered
/// HTML would break the layout.
pub fn snippet(markdown: &str, max_chars: usize) -> String {
    let mut text = String::new();
    for event in Parser::new_ext(markdown, options()) {
        match event {
            Event::Text(t) | Event::Code(t) => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak | Event::End(_) => text.push(' '),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max_chars {
        return text;
    }
    // Truncate by chars, never by bytes: slicing mid-codepoint panics.
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_lists_and_fenced_code() {
        let html = render("## Title\n\n- one\n- two\n\n```bash\nls -la\n```");
        assert!(html.contains("<h2>"));
        assert!(html.contains("<li>one</li>"));
        assert!(html.contains("<code"));
        assert!(html.contains("ls -la"));
    }

    #[test]
    fn verbatim_keeps_the_line_structure_markdown_would_flatten() {
        // A table of contents lifted out of a PDF: every entry is its own
        // line, and read as markdown they collapse into one paragraph whose
        // leader dots then stretch the width of the card.
        let html = render_verbatim("Dateiattribute\n.........24\n\nSlack\n.....24");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("Dateiattribute\n.........24"), "{html}");
        assert!(!html.contains("<p>"), "{html}");
    }

    #[test]
    fn verbatim_is_shown_as_written_not_read_as_markup() {
        // Source text, not model output: `#` is a number sign the document
        // used, and `<b>` is four characters it contains.
        let html = render_verbatim("# 3 auf Seite 12\n<b>not bold</b>");
        assert!(!html.contains("<h1"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("# 3 auf Seite 12"), "{html}");
    }

    #[test]
    fn renders_tables() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<table>"), "{html}");
    }

    #[test]
    fn strips_script_tags_from_llm_written_markdown() {
        // Chunk text is model output rendered into the operator's own
        // authenticated session. Unsanitized, this is stored XSS.
        let html = render("normal text\n\n<script>alert(1)</script>");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("alert(1)"), "{html}");
    }

    #[test]
    fn strips_event_handler_attributes() {
        let html = render("<img src=x onerror=\"alert(1)\">");
        assert!(!html.contains("onerror"), "{html}");
    }

    #[test]
    fn strips_javascript_urls() {
        let html = render("[click](javascript:alert(1))");
        assert!(!html.contains("javascript:"), "{html}");
    }

    #[test]
    fn strips_data_urls_too() {
        // data: can carry script in some contexts; the scheme allowlist must
        // not quietly permit it.
        let html = render("[click](data:text/html;base64,PHNjcmlwdD4=)");
        assert!(!html.contains("data:text/html"), "{html}");
    }

    #[test]
    fn keeps_ordinary_links_and_marks_them_safe_to_follow() {
        let html = render("[docs](https://example.com/page)");
        assert!(html.contains("https://example.com/page"));
        assert!(
            html.contains("noopener"),
            "external links need rel=noopener: {html}"
        );
    }

    #[test]
    fn code_content_is_escaped_not_executed() {
        // A chunk documenting an XSS payload must render as text.
        let html = render("Payload:\n\n```html\n<script>alert(1)</script>\n```");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn snippet_returns_plain_text_and_truncates_on_a_char_boundary() {
        let s = snippet("## Title\n\nSome **bold** text with `code`.", 20);
        assert!(!s.contains('#'));
        assert!(!s.contains('*'));
        assert!(s.chars().count() <= 21, "got {s:?}");

        // Multi-byte input must not panic or split a character.
        let s = snippet("äöü ßüber alles und noch viel mehr text", 5);
        assert!(s.chars().count() <= 6);
    }
}
