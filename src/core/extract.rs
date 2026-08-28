use crate::error::{Error, Result};
use htmd::element_handler::Handlers;
use htmd::{Element, HtmlToMarkdown};
use std::sync::LazyLock;

/// The converter, assembled once.
///
/// The handler table is fixed, so building it per capture would be work
/// repeated for nothing, and `HtmlToMarkdown` is `Send + Sync`.
static CONVERTER: LazyLock<HtmlToMarkdown> = LazyLock::new(|| {
    HtmlToMarkdown::builder()
        .add_handler(vec!["a"], |handlers: &dyn Handlers, el: Element| {
            // An in-page link means nothing once the page is a corpus: there
            // is no page left for `#NAME` to jump within. man7 and many docs
            // sites write `<h2><a id="NAME"></a>NAME <a href="#top">[top]</a>
            // </h2>`, and rendered faithfully that heading became the title
            // `[](#NAME)NAME [top](#top_of_page)` on every card in the
            // product. So: an anchor with no text is nothing; an in-page
            // anchor is its text; and an in-page anchor inside a heading that
            // follows the heading's own text is the back-link, and is nothing
            // too. An ordinary link is the ordinary link.
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
        .add_handler(vec!["img"], |_: &dyn Handlers, el: Element| {
            // A capture is text, so the image itself is not stored — but its
            // `alt` is not always decoration. Wikipedia renders every equation
            // as an image whose alt is the TeX, so dropping the element whole
            // deletes the mathematics out of an article and leaves the prose
            // around it pointing at formulae that are no longer there.
            //
            // `None` drops the element entirely, which is what an empty or
            // whitespace-only alt deserves: a spacer, a tracking pixel, or the
            // grey placeholder a news site ships ahead of the real photograph.
            el.attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "alt")
                .map(|a| a.value.trim())
                .filter(|alt| !alt.is_empty())
                .map(|alt| escape_alt(alt).into())
        })
        .build()
});

/// Whether an anchor is a heading's back-link: inside an `h1`–`h6`, after the
/// heading's own text. The `[top]` at the end of every man7 section heading.
fn is_back_link(node: &std::rc::Rc<htmd::Node>) -> bool {
    use markup5ever_rcdom::NodeData;
    let parent = node.parent.take();
    node.parent.set(parent.clone());
    let Some(parent) = parent.and_then(|w| w.upgrade()) else {
        return false;
    };
    let heading = matches!(&parent.data, NodeData::Element { name, .. }
        if matches!(name.local.as_ref(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6"));
    if !heading {
        return false;
    }
    fn has_text(node: &std::rc::Rc<htmd::Node>) -> bool {
        match &node.data {
            NodeData::Text { contents } => !contents.borrow().trim().is_empty(),
            _ => node.children.borrow().iter().any(has_text),
        }
    }
    parent
        .children
        .borrow()
        .iter()
        .take_while(|c| !std::rc::Rc::ptr_eq(c, node))
        .any(has_text)
}

/// An `alt` as an inline text node rather than as markdown.
///
/// htmd escapes the text nodes it walks itself, but whatever a handler returns
/// is already-translated markdown as far as it is concerned and goes into the
/// output verbatim. So `alt="# Overview"` on a decorative image lands as a
/// real ATX heading, and `src/infer/split.rs` splits the artifact at a
/// boundary the document never had — the same silent loss `setext_to_atx`
/// below exists to prevent, arriving from the other direction. The equations
/// this handler was written for are the second half of it: `{\displaystyle
/// E=mc^{2}}` is a run of `\`, `_` and `{}` that markdown reads as emphasis.
///
/// Whitespace collapses first. An alt is one run of text wherever it sits in
/// the prose, and a newline inside it would let the remainder open a block on
/// a line of its own, past every leading-character check below.
fn escape_alt(alt: &str) -> String {
    let line = alt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(line.len() + 8);
    // Only the first character can open a block, and only the markers that
    // open one unconditionally are escaped here: a `-` or `+` needs the space
    // after it, and htmd itself escapes no more than this.
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
    // An ordered item is a digit run and then `.` or `)`, and the escape goes
    // before the dot: `\1` is not an escape, it is a backslash and a one.
    if first.is_some_and(|c| c.is_ascii_digit())
        && let Some(i) = out.find(|c: char| !c.is_ascii_digit())
        && matches!(out.as_bytes()[i], b'.' | b')')
    {
        out.insert(i, '\\');
    }
    out
}

/// Rewrite setext headings as ATX ones.
///
/// A setext heading is underlined — `Title` over `-----` — which is valid
/// markdown and invisible to `src/infer/split.rs`, whose `is_heading` matches
/// a leading `#` and nothing else. A document written that way hands the
/// splitter no boundaries at exactly the two levels an article uses most,
/// which is the loss this module exists to prevent. Cheaper and far more
/// predictable than teaching the splitter a second heading syntax it would
/// then have to carry through windowing and line numbering.
///
/// `htmd` emits ATX for every level, so on the capture path this is now a
/// guard rather than a repair: it fires on nothing the current converter
/// produces. It is kept because the cost is one pass over lines already in
/// memory and the failure it prevents is silent — a converter that changed
/// its heading style would cost every artifact downstream a worse slice with
/// nothing in the output looking wrong.
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
                // quoting ~~~ does not end early — and only a bare one, since
                // a closing fence may carry no info string. Without that, a
                // block quoting "```yaml" ends on the quote and everything
                // after it is read as prose: the `---` two lines down becomes
                // a heading and is deleted, which is the failure this whole
                // function is here to stop.
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

/// Whether this line closes a block opened with `marker`.
///
/// An opening fence may name a language; a closing one may not. That asymmetry
/// is what keeps a snippet quoting another snippet's fence from ending the
/// block it is inside.
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
/// `html_to_markdown` off the async runtime's threads: the two parsers are a
/// synchronous walk over whatever a page contained, seconds during which
/// search, health and the queue poll on that thread would all wait.
pub async fn extract(html: String, url: Option<url::Url>, min_chars: usize) -> Result<String> {
    tokio::task::spawn_blocking(move || html_to_markdown(&html, url.as_ref(), min_chars))
        .await
        // A `JoinError` is a panic in `dom_smoothie` or `htmd` — two parsers
        // fed whatever a remote page contained — or a cancelled runtime.
        // Neither is anything the caller did, so it must not come back as a
        // 400 telling them their page was malformed while the crash goes
        // unrecorded.
        .map_err(|e| Error::Internal(format!("extraction did not finish: {e}")))?
}

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

    let converted = CONVERTER
        .convert(&content)
        .map_err(|e| Error::Validation(format!("could not read the page: {e}")))?;
    let markdown = setext_to_atx(&converted).trim().to_string();

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

        // Nor by a quoted *opening* fence of its own kind: a closing fence may
        // carry no info string, so "```yaml" inside a block is content. Read
        // as a closer it would end the block three lines early and the `---`
        // below would be eaten as a heading.
        let quoted = "```\n```yaml\nfoo\n---\nbar\n```";
        assert_eq!(setext_to_atx(quoted), quoted);

        // A closer may still be padded, indented up to three, or longer than
        // the fence that opened it.
        let padded = "```\nfoo\n---\n  ```  ";
        assert_eq!(setext_to_atx(padded), padded);
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
    fn a_citation_leaves_its_marker_and_none_of_its_markup() {
        // Parsoid wraps every Wikipedia reference in a `<sup>` carrying a few
        // hundred characters of `data-mw` JSON. The converter this module used
        // before serialised that subtree back into the corpus verbatim — on one
        // article, 43% of the stored bytes were markup of this shape, and it
        // reached the passages, the artifacts and the paste.
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
    fn in_page_anchors_leave_a_heading_as_its_own_words() {
        // man7's section headings, verbatim: an empty anchor carrying the id,
        // the name, and a back-link to the top of the page. Sixteen artifacts
        // titled `[](#NAME)NAME [top](#top_of_page)` came out of one man page.
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
        // An in-page link in prose keeps its words; a real link stays a link.
        assert!(md.contains("See the name and"), "{md}");
        assert!(md.contains("[elsewhere](https://x.test/)"), "{md}");
        assert!(!md.contains("[]("), "{md}");
    }

    #[test]
    fn a_heading_nested_in_a_section_still_reaches_the_splitter() {
        // The failure that cost more than the markup did: on identical
        // readability output the previous converter recovered no headings at
        // all from pages that wrap each one in a `<section>`, so every window
        // boundary fell back to a blank line and every artifact downstream was
        // drawn from a slice the segmenter had to guess the edges of.
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
        // Wikipedia renders every equation as an image whose alt is the TeX.
        // Dropping the element whole deletes the mathematics out of the
        // article; keeping the tag puts markup back in the corpus.
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
        // A handler's output is inserted verbatim, so an alt is the one text
        // on this path htmd does not escape. `src/infer/split.rs` splits on a
        // leading `#`, which would put a segment boundary in the middle of a
        // paragraph on the word of a decorative image.
        let html = "<html><body><article>\
            <p>A paragraph long enough that readability scores this document \
            as content rather than as furniture, which is the whole of what \
            this sentence is here to do.</p>\
            <p><img src=\"/rule.png\" alt=\"# Overview\"> opens the section, \
            and the prose runs on from there for a while yet.</p>\
            </article></body></html>";
        let md = html_to_markdown(html, None, 10).unwrap();
        assert!(md.contains("Overview"), "the alt was dropped:\n{md}");
        // The condition `is_heading` in that module matches, spelled out
        // here because it is private to it.
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
