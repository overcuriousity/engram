use crate::error::{Error, Result};
use htmd::element_handler::Handlers;
use htmd::{Element, HtmlToMarkdown};
use std::sync::LazyLock;

/// Built once: the handler table is fixed and `HtmlToMarkdown` is `Send + Sync`.
static CONVERTER: LazyLock<HtmlToMarkdown> = LazyLock::new(|| {
    HtmlToMarkdown::builder()
        .add_handler(vec!["a"], |handlers: &dyn Handlers, el: Element| {
            // An in-page link means nothing in a corpus: an empty anchor is nothing, an in-page one
            // is its text, and a heading's back-link (man7's `[top]`) is nothing too.
            let href = el
                .attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "href")
                .map(|a| a.value.as_ref());
            let content = handlers.walk_children(el.node);
            if content.content.trim().is_empty() {
                return None;
            }
            if !href.is_some_and(|h| h.starts_with('#')) {
                return handlers.fallback(el);
            }
            if is_back_link(el.node) {
                return None;
            }
            Some(content)
        })
        .add_handler(
            vec!["b", "strong", "i", "em"],
            |handlers: &dyn Handlers, el: Element| {
                // A `<pre>` becomes a literal code block, where htmd's `**` for `<b>` would show as
                // asterisks.
                match has_ancestor(el.node, "pre") {
                    true => Some(handlers.walk_children(el.node)),
                    false => handlers.fallback(el),
                }
            },
        )
        .add_handler(vec!["img"], |_: &dyn Handlers, el: Element| {
            // Wikipedia equations are images whose alt is the TeX; an empty alt is a spacer or
            // pixel and is dropped.
            el.attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "alt")
                .map(|a| a.value.trim())
                .filter(|alt| !alt.is_empty())
                .map(|alt| escape_alt(alt).into())
        })
        .build()
});

/// `parent` is a `Cell` of a weak handle, so it is taken and put back rather than borrowed.
fn parent_of(node: &std::rc::Rc<htmd::Node>) -> Option<std::rc::Rc<htmd::Node>> {
    let parent = node.parent.take();
    node.parent.set(parent.clone());
    parent.and_then(|w| w.upgrade())
}

fn tag_of(node: &std::rc::Rc<htmd::Node>) -> Option<String> {
    match &node.data {
        markup5ever_rcdom::NodeData::Element { name, .. } => Some(name.local.to_string()),
        _ => None,
    }
}

fn has_ancestor(node: &std::rc::Rc<htmd::Node>, tag: &str) -> bool {
    let mut cur = parent_of(node);
    while let Some(n) = cur {
        if tag_of(&n).as_deref() == Some(tag) {
            return true;
        }
        cur = parent_of(&n);
    }
    false
}

/// Inside a heading, after its text, nothing after it, and written like one (`[top]`, `top`, an
/// arrow); never numbered, since a bracketed number is a citation.
fn is_back_link(node: &std::rc::Rc<htmd::Node>) -> bool {
    use markup5ever_rcdom::NodeData;
    let Some(parent) = parent_of(node) else {
        return false;
    };
    let heading = matches!(
        tag_of(&parent).as_deref(),
        Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6")
    );
    if !heading {
        return false;
    }
    fn has_text(node: &std::rc::Rc<htmd::Node>) -> bool {
        match &node.data {
            NodeData::Text { contents } => !contents.borrow().trim().is_empty(),
            _ => node.children.borrow().iter().any(has_text),
        }
    }
    fn text_of(node: &std::rc::Rc<htmd::Node>) -> String {
        match &node.data {
            NodeData::Text { contents } => contents.borrow().to_string(),
            _ => node.children.borrow().iter().map(text_of).collect(),
        }
    }
    let children = parent.children.borrow();
    let after_text = children
        .iter()
        .skip_while(|c| !std::rc::Rc::ptr_eq(c, node))
        .skip(1)
        .any(has_text);
    let before_text = children
        .iter()
        .take_while(|c| !std::rc::Rc::ptr_eq(c, node))
        .any(has_text);
    // Text before it and none after it: a back-link ends a heading.
    if !before_text || after_text {
        return false;
    }
    let text = text_of(node);
    let t = text.trim();
    // Bounded: a heading can end in a bracketed citation such as `[RFC 2119]`.
    if t.chars().count() > 8 {
        return false;
    }
    if let Some(inner) = t.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        // A digit makes it a reference (`[1]`, `[§4.2]`); an arrow `[↑]` is still a back-link.
        return !inner.chars().any(|c| c.is_numeric());
    }
    let word = t
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    word.is_empty() || matches!(word.as_str(), "top" | "back" | "up")
}

/// A handler's output is inserted verbatim, so an alt like `# Overview` would become a heading for
/// `infer::split`. Whitespace collapses first, so a newline cannot open a block past the
/// leading-character checks.
fn escape_alt(alt: &str) -> String {
    let line = alt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(line.len() + 8);
    // Only the first character can open a block; `-` and `+` need a following space, as in htmd.
    let first = line.chars().next();
    if matches!(first, Some('=' | '~' | '>' | '#'))
        || (matches!(first, Some('-' | '+')) && line.chars().nth(1) == Some(' '))
    {
        out.push('\\');
    }
    for ch in line.chars() {
        if matches!(ch, '\\' | '*' | '_' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(ch);
    }
    // The escape goes before the dot: `\1` is a backslash and a one, not an escape.
    if first.is_some_and(|c| c.is_ascii_digit())
        && let Some(i) = out.find(|c: char| !c.is_ascii_digit())
        && matches!(out.as_bytes()[i], b'.' | b')')
    {
        out.insert(i, '\\');
    }
    out
}

/// Rewrite setext headings as ATX: `infer::split` only sees a leading `#`. A guard, since htmd
/// emits ATX. Fenced code is left exactly as it arrived: a `---` there is content.
fn setext_to_atx(md: &str) -> String {
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut fence: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        match fence {
            Some(open) => {
                // Only a bare closer of the opening marker closes, so a quoted "```yaml" or ~~~
                // does not end the block.
                if closes(line, open) {
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
        // A `---` under a blank line is a thematic break, not a heading.
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

/// Up to three leading spaces, per CommonMark; a fourth makes it indented code.
fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m))
}

/// A closing fence may not carry an info string, unlike an opening one.
fn closes(line: &str, marker: &str) -> bool {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    trimmed.strip_prefix(marker).is_some_and(|rest| {
        rest.trim_end()
            .chars()
            .all(|c| c == marker.as_bytes()[0] as char)
    })
}

/// A page reduced to markdown, and readability's title for it (`None` when the page had none).
/// Extraction stays synchronous: `Readability` is `!Send` and must never live across an `.await`.
pub struct Extracted {
    pub title: Option<String>,
    pub markdown: String,
}

pub async fn extract(html: String, url: Option<url::Url>, min_chars: usize) -> Result<Extracted> {
    tokio::task::spawn_blocking(move || html_to_document(&html, url.as_ref(), min_chars))
        .await
        // A `JoinError` is a parser panic or a cancelled runtime: an internal error, not the
        // caller's 400.
        .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?
}

pub fn html_to_markdown(
    html: &str,
    base_url: Option<&url::Url>,
    min_chars: usize,
) -> Result<String> {
    html_to_document(html, base_url, min_chars).map(|d| d.markdown)
}

/// Readability drops the page's `<h1>`, so without the title the first section heading names the
/// corpus.
pub fn html_to_document(
    html: &str,
    base_url: Option<&url::Url>,
    min_chars: usize,
) -> Result<Extracted> {
    let (title, content) = {
        let mut readability =
            dom_smoothie::Readability::new(html, base_url.map(url::Url::as_str), None)
                .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
        let article = readability
            .parse()
            .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
        let title = article
            .title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        (
            (!title.is_empty()).then_some(title),
            article.content.to_string(),
        )
    };

    let converted = CONVERTER
        .convert(&content)
        .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
    let markdown = setext_to_atx(&converted).trim().to_string();

    // An empty extraction is refused even for a selection, which is exempt from the floor.
    if markdown.is_empty() {
        return Err(Error::Validation(
            "nothing survived extraction — there was no text in what was captured".into(),
        ));
    }

    // A server-side GET is served a login wall with a 200; counting what survived turns that into
    // an error.
    let extracted = markdown.chars().count();
    if extracted < min_chars {
        return Err(Error::Validation(format!(
            "only {extracted} characters extracted, below the {min_chars} the capture needs — \
             the page was probably a login wall or an empty shell"
        )));
    }
    Ok(Extracted { title, markdown })
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
        let md = html_to_markdown(ARTICLE, None, 10).unwrap();
        assert!(
            md.contains("## Read-only first"),
            "the h2 must survive as a markdown heading, got:\n{md}"
        );
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
        let fenced = "before\n\n```yaml\nfoo\n---\nbar\n```\n\nafter";
        assert_eq!(setext_to_atx(fenced), fenced);

        let tilde = "~~~\ntitle\n===\n~~~";
        assert_eq!(setext_to_atx(tilde), tilde);

        let nested = "```\n~~~\nfoo\n---\n```\n\nSection\n---";
        assert_eq!(
            setext_to_atx(nested),
            "```\n~~~\nfoo\n---\n```\n\n## Section"
        );

        let quoted = "```\n```yaml\nfoo\n---\nbar\n```";
        assert_eq!(setext_to_atx(quoted), quoted);

        let padded = "```\nfoo\n---\n  ```  ";
        assert_eq!(setext_to_atx(padded), padded);
    }

    #[test]
    fn a_heading_after_a_code_block_is_still_promoted() {
        let md = "```\ncode\n```\n\nRead-only first\n---\n\nbody";
        assert_eq!(
            setext_to_atx(md),
            "```\ncode\n```\n\n## Read-only first\n\nbody"
        );
    }

    #[test]
    fn a_citation_leaves_its_marker_and_none_of_its_markup() {
        let cited = format!(
            "<html><body><article><p>{}</p>\
             <p><sup about=\"#mwt4\" typeof=\"mw:Extension/ref\" \
             data-mw=\"{{&quot;name&quot;:&quot;ref&quot;}}\">\
             <a href=\"#cite_note-1\"><span><span>[</span>1<span>]</span></span></a>\
             </sup></p></article></body></html>",
            "The association provides an interface that lets the file stand in \
             for a block special file, and this paragraph has to be long enough \
             that readability scores it as content rather than as furniture."
        );
        let md = html_to_markdown(&cited, None, 10).unwrap();
        assert!(!md.contains('<'), "markup survived extraction:\n{md}");
        assert!(!md.contains("data-mw"), "attribute JSON survived:\n{md}");
        assert!(
            md.contains('1'),
            "the citation marker itself was lost:\n{md}"
        );
    }

    #[test]
    fn bold_inside_a_pre_block_is_plain_text_not_asterisks() {
        let page = format!(
            "<html><head><title>losetup(8) - Linux manual page</title></head><body><article>\
             <h2>OPTIONS</h2><pre>       <b>-a</b>, <b>--all</b>\n           Show the status of all loop \
             devices. <i>loopdev</i> names one.\n{}</pre>\
             <p>Outside the block <b>bold</b> is still bold, and {}</p>\
             </article></body></html>",
            "           more lines of the manual page so that readability keeps it as content\n"
                .repeat(6),
            "this paragraph is long enough to be kept as content by the readability pass too."
        );
        let doc = html_to_document(&page, None, 10).unwrap();
        assert!(!doc.markdown.contains("**-a**"), "{}", doc.markdown);
        assert!(doc.markdown.contains("-a, --all"), "{}", doc.markdown);
        assert!(!doc.markdown.contains("*loopdev*"), "{}", doc.markdown);
        assert!(doc.markdown.contains("**bold**"), "{}", doc.markdown);
        assert!(
            doc.title
                .as_deref()
                .is_some_and(|t| t.starts_with("losetup(8)")),
            "{:?}",
            doc.title
        );
    }

    #[test]
    fn in_page_anchors_leave_a_heading_as_its_own_words() {
        let page = format!(
            "<html><body><article>\
             <h2><a id=\"NAME\"></a>NAME         <a href=\"#top_of_page\">[top]</a></h2>\
             <p>{}</p>\
             <h2><a id=\"OPTIONS\"></a>OPTIONS         <a href=\"#top_of_page\">[top]</a></h2>\
             <p>See <a href=\"#NAME\">the name</a> and <a href=\"https://x.test/\">elsewhere</a>.</p>\
             <p>{}</p>\
             </article></body></html>",
            "losetup is used to associate loop devices with regular files or block \
             devices, to detach loop devices, and to query the status of a loop device, \
             which is enough prose for readability to keep the section.",
            "the options are many and this paragraph exists to be long enough to be \
             kept as content by the readability pass rather than dropped as furniture.",
        );
        let md = html_to_markdown(&page, None, 10).unwrap();
        let headings: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(headings, vec!["## NAME", "## OPTIONS"], "{md}");
        assert!(md.contains("See the name and"), "{md}");
        assert!(md.contains("[elsewhere](https://x.test/)"), "{md}");
        assert!(!md.contains("[]("), "{md}");
    }

    #[test]
    fn a_heading_that_ends_in_an_ordinary_in_page_link_keeps_its_words() {
        let page = format!(
            "<html><body><article>\
             <h2>Configuration and the <a href=\"#flags\">flag list</a></h2>\
             <p>{}</p>\
             <h2>Recovery <a href=\"#top\">top</a></h2>\
             <p>{}</p>\
             </article></body></html>",
            "the configuration is read from one file and every flag in it has a long \
             form and a short form, which is enough prose for the readability pass to \
             keep this section rather than drop it as furniture.",
            "recovery walks the journal from the last checkpoint forward and replays \
             every record it finds there, which is again long enough to be kept as \
             content rather than dropped.",
        );
        let md = html_to_markdown(&page, None, 10).unwrap();
        let headings: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            headings,
            vec!["## Configuration and the flag list", "## Recovery"],
            "{md}"
        );
    }

    #[test]
    fn a_heading_that_ends_in_a_bracketed_citation_keeps_its_words() {
        let body = |what: &str| {
            format!(
                "each {what} is stated once and referred to from everywhere else in \
                 the document, which is enough prose for the readability pass to keep \
                 this section rather than drop it as furniture."
            )
        };
        let page = format!(
            "<html><body><article>\
             <h2>Requirement levels <a href=\"#refs\">[RFC 2119]</a></h2>\
             <p>{}</p>\
             <h2>Retry budgets <a href=\"#n1\">[1]</a></h2>\
             <p>{}</p>\
             <h2>Error codes <a href=\"#top\">[top]</a></h2>\
             <p>{}</p>\
             <h2>Wire format <a href=\"#top\">[↑]</a></h2>\
             <p>{}</p>\
             </article></body></html>",
            body("requirement"),
            body("budget"),
            body("code"),
            body("frame"),
        );
        let md = html_to_markdown(&page, None, 10).unwrap();
        let headings: Vec<&str> = md.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            headings,
            vec![
                "## Requirement levels \\[RFC 2119\\]",
                "## Retry budgets \\[1\\]",
                "## Error codes",
                "## Wire format",
            ],
            "{md}"
        );
    }

    #[test]
    fn a_heading_nested_in_a_section_still_reaches_the_splitter() {
        let sectioned = "<html><body><article>\
            <section><h2>Read-only first</h2>\
            <p>Always mount read-only until you have a hash of the source \
               image, because a mount that replays a dirty journal writes to \
               the evidence you were trying to preserve.</p></section>\
            </article></body></html>";
        let md = html_to_markdown(sectioned, None, 10).unwrap();
        assert!(
            md.contains("## Read-only first"),
            "the heading must survive as an ATX one, got:\n{md}"
        );
    }

    #[test]
    fn an_images_alt_is_kept_as_text_and_an_empty_one_is_dropped() {
        let with_math = "<html><body><article>\
            <p>The relation is written \
            <img src=\"/math/render/svg/2f92\" width=\"9\" height=\"2\" \
                 alt=\"{\\displaystyle E=mc^{2}.}\"> and it holds in every \
            frame, which this sentence lengthens so readability scores the \
            paragraph as content rather than as furniture.</p>\
            <p><img src=\"/grey-placeholder.png\" alt=\"\"></p>\
            </article></body></html>";
        let md = html_to_markdown(with_math, None, 10).unwrap();
        assert!(md.contains("E=mc^{2}"), "the equation was deleted:\n{md}");
        assert!(!md.contains('<'), "markup survived extraction:\n{md}");
        assert!(
            !md.contains("grey-placeholder"),
            "an alt-less spacer left a trace:\n{md}"
        );
    }

    #[test]
    fn an_alt_cannot_invent_a_heading_the_document_never_had() {
        let html = "<html><body><article>\
            <p>A paragraph long enough that readability scores this document \
            as content rather than as furniture, which is the whole of what \
            this sentence is here to do.</p>\
            <p><img src=\"/rule.png\" alt=\"# Overview\"> opens the section, \
            and the prose runs on from there for a while yet.</p>\
            </article></body></html>";
        let md = html_to_markdown(html, None, 10).unwrap();
        assert!(md.contains("Overview"), "the alt was dropped:\n{md}");
        // `infer::split`'s private `is_heading` condition, spelled out.
        assert!(
            !md.lines().any(|l| l.trim_start().starts_with("# ")
                || l.trim_start().starts_with("#")
                    && l.trim_start().trim_start_matches('#').starts_with(' ')),
            "an alt opened a heading:\n{md}"
        );
    }

    #[test]
    fn html_that_is_not_a_document_at_all_is_an_error_not_a_panic() {
        let err = html_to_markdown("", None, 200).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}
