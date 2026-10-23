use std::sync::LazyLock;

use math_core::{LatexToMathML, MathCoreConfig, MathDisplay};
use pulldown_cmark::{CowStr, Event, Options, Parser, html};

fn options() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_FOOTNOTES);
    // `$…$` and `$$…$$`. A price written `5$ und 10$` is not math: an opening
    // `$` followed by a space never opens one.
    o.insert(Options::ENABLE_MATH);
    o
}

/// One converter for the process. Building it compiles the command table, and
/// converting with local state keeps one formula's `\def` out of the next.
static LATEX: LazyLock<LatexToMathML> = LazyLock::new(|| {
    LatexToMathML::new(MathCoreConfig::default()).expect("the default config has no macros to fail")
});

/// MathML Core, the subset every current browser draws without a script.
///
/// Exactly what `math-core` emits, minus `style` and `id`: `style` would hand
/// model-written raw HTML a way to position things over the page, and costs
/// only `\color`.
const MATHML_TAGS: &[&str] = &[
    "math", "semantics", "annotation", "mrow", "mi", "mn", "mo", "mtext", "mspace", "msub",
    "msup", "msubsup", "munder", "mover", "munderover", "mmultiscripts", "mprescripts", "mfrac",
    "msqrt", "mroot", "mtable", "mtr", "mtd", "mpadded", "mphantom", "menclose", "merror",
];
const MATHML_ATTRIBUTES: &[&str] = &[
    "display", "mathvariant", "accent", "accentunder", "displaystyle", "scriptlevel", "form",
    "stretchy", "symmetric", "largeop", "movablelimits", "lspace", "rspace", "minsize", "maxsize",
    "linethickness", "width", "height", "notation", "encoding",
];

/// A formula as MathML, or its source in a code span when it does not parse.
///
/// The source rather than nothing: a command `math-core` does not know is still
/// what the answer said, and shown as written it is readable to anyone who
/// reads LaTeX.
fn math(latex: &str, display: MathDisplay) -> Event<'static> {
    match LATEX.convert_with_local_state(latex, display) {
        Ok(out) => Event::Html(CowStr::from(out.mathml)),
        Err(_) => Event::Code(CowStr::from(latex.to_string())),
    }
}

/// Render chunk markdown to HTML, then sanitize.
///
/// Chunk text is written by a language model and displayed inside the
/// operator's authenticated session, so it is untrusted input by definition.
/// Sanitizing after rendering catches both raw HTML passed through by the
/// markdown parser and anything the parser itself emits — the MathML included.
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
        .add_tag_attributes("math", ["display"])
        .add_generic_attributes(["class"])
        .add_generic_attributes(MATHML_ATTRIBUTES)
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

/// Truncate at a word, never inside one, and never inside a codepoint.
///
/// `chars().take(n)` was the whole of this, and it is what produced "…darin
/// vo" in the sitting: a name cut mid-word reads as a broken name, where one
/// cut at a space reads as an opening. Falls back to the hard cut when there
/// is no space to fall back to — one unbroken 200-character token is still
/// better shortened than shown whole — and refuses a break so early that a
/// single word would stand for a whole passage.
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

/// A name for something that has none, derived from its own opening.
///
/// A verbatim passage has no title by design. Where the layout can let a
/// snippet speak for the row there should be no heading at all — see
/// `render_hit`. Where a name is structurally required, a button label or a
/// list of what this sitting touched, this is that name: the body's opening,
/// with the markup and the leading punctuation that are structure rather than
/// subject taken off the front. `Keep "- schneller Schreibzugriff (…) -"` is
/// what the dedupe queue offered without this.
pub fn stand_in_title(text: &str, max_chars: usize) -> String {
    let flat = snippet(text, usize::MAX);
    let opening = flat.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '-' | '–' | '—' | '*' | '#' | '>' | '·' | '•' | '|')
    });
    // Put back a number the parse took for structure. `snippet` is a whole
    // CommonMark read that keeps the text events, so `1. Einleitung` arrives
    // here as `Einleitung`: an ordered-list marker is structure to the parser
    // and part of the name to everyone else. It matters twice over now that
    // stored titles come through here too — two syntheses called `1. Einleitung`
    // and `2. Einleitung` were shown under one name, and the number is the
    // whole of what told them apart.
    let name = match leading_enumerator(text) {
        Some(marker) => format!("{marker} {opening}"),
        None => opening.to_string(),
    };
    truncate_at_word(name.trim(), max_chars)
}

/// The `1.` or `1)` a text opens with, if it opens with one.
///
/// Only what CommonMark itself would have eaten: digits, one `.` or `)`, and
/// whitespace after it. A date written `1.9. Termin` has no space after the
/// first dot and is not a list to the parser either, so both leave it whole.
/// Returns the marker as the text wrote it — `10.` stays `10.`, and `)` does not
/// come back as `.` — because a name is what was written.
fn leading_enumerator(text: &str) -> Option<&str> {
    let start = text.trim_start();
    let digits = start.len() - start.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    // Nine is CommonMark's own limit on how long an ordered marker may be; past
    // it the parser stops reading a list, and so does this.
    if digits == 0 || digits > 9 {
        return None;
    }
    let delim = start[digits..].chars().next()?;
    if !matches!(delim, '.' | ')') {
        return None;
    }
    // A marker with nothing after it is not a list item, and a title that is
    // only `3.` has nothing for this to be the marker of.
    let marker = &start[..digits + delim.len_utf8()];
    match start[marker.len()..].starts_with(|c: char| c.is_whitespace()) {
        true => Some(marker),
        false => None,
    }
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
    fn renders_inline_and_display_math_as_mathml() {
        // The answer that asked for this: a learning rule written the way a
        // model writes one, inline and displayed.
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
        // The sitting rendered "…zusätzlich darin vo" — a name cut mid-word
        // reads as a truncated name, not as the opening of a passage.
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
        // `Keep "- schneller Schreibzugriff (…) -"` was a body opening,
        // dashes and all, pressed into service as a name.
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
        // The number is structure to the parser and part of the name to
        // everyone else: without it two syntheses are shown under one name.
        assert_eq!(stand_in_title("1. Einleitung", 60), "1. Einleitung");
        assert_eq!(stand_in_title("2. Einleitung", 60), "2. Einleitung");
        // As written: the width of the number and the delimiter both survive.
        assert_eq!(stand_in_title("10. Kapitel Zehn", 60), "10. Kapitel Zehn");
        assert_eq!(stand_in_title("1) Foo", 60), "1) Foo");
        // A marker inside a heading was never a list marker; the heading's own
        // `#` still goes, and the text under it is untouched either way.
        assert_eq!(stand_in_title("## 1. Einleitung", 60), "1. Einleitung");
    }

    #[test]
    fn a_number_that_is_not_a_marker_is_left_alone() {
        // No space after the dot: not a list to CommonMark, and not one here.
        assert_eq!(
            stand_in_title("1.9. Termin im Mai", 60),
            "1.9. Termin im Mai"
        );
        // Nothing for the marker to mark: an empty list item, which flattens
        // to nothing at all, exactly as `---` does. `title_of` falls back to
        // the id there rather than showing a name that is one number.
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

