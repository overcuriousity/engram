use std::sync::LazyLock;

use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
use pulldown_cmark::{CowStr, Event, Options, Parser, html};

fn options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_FOOTNOTES);
    // `$…$` and `$$…$$`; an opening `$` followed by a space never opens math, so prices are safe.
    o.insert(Options::ENABLE_MATH);
    o
}

/// One converter: building it compiles the command table, and local state keeps one formula's `\def` out of the next.
static LATEX: LazyLock<Option<LatexToMathML>> =
    LazyLock::new(|| LatexToMathML::new(MathCoreConfig::default()).ok());

/// MathML Core minus `style` and `id`: `style` would let model-written HTML position things over the page.
const MATHML_TAGS: &[&str] = &[
    "math",
    "semantics",
    "annotation",
    "mrow",
    "mi",
    "mn",
    "mo",
    "mtext",
    "mspace",
    "msub",
    "msup",
    "msubsup",
    "munder",
    "mover",
    "munderover",
    "mmultiscripts",
    "mprescripts",
    "mfrac",
    "msqrt",
    "mroot",
    "mtable",
    "mtr",
    "mtd",
    "mpadded",
    "mphantom",
    "menclose",
    "merror",
];
const MATHML_ATTRIBUTES: &[&str] = &[
    "display",
    "mathvariant",
    "accent",
    "accentunder",
    "displaystyle",
    "scriptlevel",
    "form",
    "stretchy",
    "symmetric",
    "largeop",
    "movablelimits",
    "lspace",
    "rspace",
    "minsize",
    "maxsize",
    "linethickness",
    "width",
    "height",
    "notation",
    "encoding",
];

/// The source in a code span when it does not parse, since it is still what the answer said.
fn math(latex: &str, display: MathDisplay) -> Event<'static> {
    match LATEX
        .as_ref()
        .map(|l| l.convert_with_local_state(latex, display))
    {
        Some(Ok(out)) => Event::Html(CowStr::from(out.mathml)),
        _ => Event::Code(CowStr::from(latex.to_string())),
    }
}

/// Chunk text is model-written and shown in an authenticated session, so it is sanitized after rendering.
pub fn render(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, options()).map(|event| match event {
        Event::InlineMath(latex) => math(&latex, MathDisplay::Inline),
        Event::DisplayMath(latex) => math(&latex, MathDisplay::Block),
        other => other,
    });
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, parser);

    ammonia::Builder::default()
        .link_rel(Some("noopener noreferrer nofollow"))
        .add_tags(MATHML_TAGS)
        .add_generic_attributes(["class"])
        .add_generic_attributes(MATHML_ATTRIBUTES)
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .clean(&unsafe_html)
        .to_string()
}

/// Escaped inside a `<pre>`: the line breaks are the structure.
/// Three replacements suffice for text content; `ammonia::clean_text` would entity-encode every space.
pub fn render_verbatim(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<pre class=\"verbatim\">{escaped}</pre>")
}

/// Markup removed, for list views where rendered HTML would break the layout.
pub fn snippet(markdown: &str, max_chars: usize) -> String {
    let mut text = String::new();
    for event in Parser::new_ext(markdown, options()) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) => {
                text.push_str(&t)
            }
            Event::SoftBreak | Event::HardBreak | Event::End(_) => text.push(' '),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_at_word(&text, max_chars)
}

/// Falls back to a hard cut when there is no space, and refuses a break so early one word stands for a passage.
pub fn truncate_at_word(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // Truncate by chars, never by bytes: slicing mid-codepoint panics.
    let hard: String = text.chars().take(max_chars).collect();
    let cut = match hard.rfind(char::is_whitespace) {
        Some(i) if hard[..i].chars().count() * 2 >= max_chars => &hard[..i],
        _ => hard.as_str(),
    };
    format!("{}…", cut.trim_end())
}

/// The body's opening minus leading markup and punctuation, for where a name is structurally required.
pub fn stand_in_title(text: &str, max_chars: usize) -> String {
    let flat = snippet(text, usize::MAX);
    let opening = flat.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '-' | '–' | '—' | '*' | '#' | '>' | '·' | '•' | '|')
    });
    // Put back the list number the parse took for structure: `1. Einleitung` and `2. Einleitung` differ only by it.
    let name = match leading_enumerator(text) {
        Some(marker) => format!("{marker} {opening}"),
        None => opening.to_string(),
    };
    truncate_at_word(name.trim(), max_chars)
}

/// Only what CommonMark would eat (digits, `.` or `)`, whitespace), returned as written.
fn leading_enumerator(text: &str) -> Option<&str> {
    let start = text.trim_start();
    let digits = start.len() - start.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    // Nine digits is CommonMark's own limit on an ordered marker.
    if digits == 0 || digits > 9 {
        return None;
    }
    let delim = start[digits..].chars().next()?;
    if !matches!(delim, '.' | ')') {
        return None;
    }
    // A marker with nothing after it is not a list item.
    let marker = &start[..digits + delim.len_utf8()];
    match start[marker.len()..].starts_with(|c: char| c.is_whitespace()) {
        true => Some(marker),
        false => None,
    }
}

/// `://` is spelled with entities, so a URL stays readable but is not a reference.
pub fn inert(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '/' => out.push_str("&#47;"),
            c => out.push(c),
        }
    }
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
        let html = render_verbatim("Dateiattribute\n.........24\n\nSlack\n.....24");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("Dateiattribute\n.........24"), "{html}");
        assert!(!html.contains("<p>"), "{html}");
    }

    #[test]
    fn verbatim_is_shown_as_written_not_read_as_markup() {
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
        // Unsanitized, this is stored XSS.
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
        // data: can carry script; the scheme allowlist must not permit it.
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
    fn renders_inline_and_display_math_as_mathml() {
        let html = render(
            "Mit $\\eta$ als Lernrate:\n\n$$w_i \\leftarrow w_i + \\eta \\cdot c \\cdot [h(x) - c(x)]$$",
        );
        assert!(html.contains("<math><mi>η</mi></math>"), "{html}");
        assert!(html.contains(r#"<math display="block">"#), "{html}");
        assert!(html.contains("<msub><mi>w</mi><mi>i</mi></msub>"), "{html}");
        assert!(!html.contains('$'), "{html}");
    }

    #[test]
    fn a_dollar_amount_is_not_math() {
        let html = render("Kostet 5$ und 10$.");
        assert!(html.contains("5$ und 10$"), "{html}");
        assert!(!html.contains("<math"), "{html}");
    }

    #[test]
    fn a_formula_that_does_not_parse_is_shown_as_its_source() {
        let html = render("Befehl: $\\nosuchcommand x$");
        assert!(html.contains("<code>\\nosuchcommand x</code>"), "{html}");
    }

    #[test]
    fn text_inside_a_formula_is_escaped() {
        let html = render("$\\text{<script>alert(1)</script>}$");
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("<mtext>"), "{html}");
    }

    #[test]
    fn raw_mathml_from_the_model_loses_style_and_handlers() {
        let html = render(
            r#"<math><mrow style="position:fixed" onclick="alert(1)" id="x"><mi>x</mi></mrow></math>"#,
        );
        assert!(!html.contains("style"), "{html}");
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("id="), "{html}");
        assert!(html.contains("<mi>x</mi>"), "{html}");
    }

    #[test]
    fn a_snippet_keeps_a_formula_as_its_source() {
        assert_eq!(snippet("Rate $\\eta$ klein", 60), "Rate \\eta klein");
    }

    #[test]
    fn a_stand_in_title_stops_at_a_word() {
        let t = stand_in_title(
            "Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort",
            60,
        );
        assert!(!t.contains("vo…"), "cut mid-word: {t:?}");
        assert!(t.ends_with('…'), "{t:?}");
        assert!(t.chars().count() <= 61, "{t:?}");
    }

    #[test]
    fn a_stand_in_title_drops_leading_punctuation_and_markup() {
        assert_eq!(
            stand_in_title("- schneller Schreibzugriff auf den Stapel", 60),
            "schneller Schreibzugriff auf den Stapel"
        );
        assert_eq!(
            stand_in_title("## 3.4.2 FESTE MFT RECORDS", 60),
            "3.4.2 FESTE MFT RECORDS"
        );
    }

    #[test]
    fn a_stand_in_title_keeps_the_number_a_section_opens_with() {
        assert_eq!(stand_in_title("1. Einleitung", 60), "1. Einleitung");
        assert_eq!(stand_in_title("2. Einleitung", 60), "2. Einleitung");
        assert_eq!(stand_in_title("10. Kapitel Zehn", 60), "10. Kapitel Zehn");
        assert_eq!(stand_in_title("1) Foo", 60), "1) Foo");
        assert_eq!(stand_in_title("## 1. Einleitung", 60), "1. Einleitung");
    }

    #[test]
    fn a_number_that_is_not_a_marker_is_left_alone() {
        assert_eq!(
            stand_in_title("1.9. Termin im Mai", 60),
            "1.9. Termin im Mai"
        );
        assert_eq!(stand_in_title("3.", 60), "");
        assert_eq!(stand_in_title("2024 war ein Jahr", 60), "2024 war ein Jahr");
    }

    #[test]
    fn a_short_body_becomes_a_stand_in_unchanged() {
        assert_eq!(
            stand_in_title("CPU fair scheduler parameter", 60),
            "CPU fair scheduler parameter"
        );
    }

    #[test]
    fn a_stand_in_of_nothing_is_empty() {
        assert_eq!(stand_in_title("   \n\n  ", 60), "");
        assert_eq!(stand_in_title("---", 60), "");
    }

    #[test]
    fn a_snippet_stops_at_a_word_too() {
        let s = snippet(
            "Die digitale Forensik unterscheidet sich zusätzlich darin von einem Tatort",
            30,
        );
        assert!(!s.contains("zusä…"), "cut mid-word: {s:?}");
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
