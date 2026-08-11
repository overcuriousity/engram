use pulldown_cmark::{Event, Options, Parser, html};

fn options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_FOOTNOTES);
    o
}

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
    fn renders_tables() {
        let html = render("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<table>"), "{html}");
    }

    #[test]
    fn strips_script_tags_from_llm_written_markdown() {
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

        let s = snippet("äöü ßüber alles und noch viel mehr text", 5);
        assert!(s.chars().count() <= 6);
    }
}
