//! Checking an answer against what the model was actually shown.

/// Literals the answer carries that appear in none of its excerpts.
///
/// The same guard `jobs::window` already applies to every synthesised artifact
/// (`verify::missing_literals`), pointed at generation instead of synthesis. A
/// number, command or path that appears in no cited excerpt is not a fact the
/// base holds — it is the model's own, and the page must say so rather than let
/// it be read as retrieved.
///
/// No inference: this is a string operation over text already generated.
pub(super) fn unsupported_literals(answer: &str, excerpts: &[String]) -> Vec<String> {
    if excerpts.is_empty() {
        return Vec::new();
    }
    crate::infer::verify::missing_literals(answer, &[], &excerpts.join("\n\n"))
        .into_iter()
        .filter(|lit| !looks_like_a_list_item(lit))
        .collect()
}

/// A candidate that is a bullet of prose rather than a literal.
///
/// `extract_literals` treats a line indented four spaces as code, which is
/// right for the reference documentation synthesis reads and wrong for an
/// answer: a bullet list nested four spaces deep is ordinary markdown, and
/// every nested bullet then arrives here as an invented literal. One answer
/// with a nested list produced three, each marking a whole sentence of prose.
///
/// The badge's whole value is being believed. A guard that fires on formatting
/// teaches the reader to dismiss it, and the fabricated command this exists for
/// gets dismissed with the noise.
///
/// The rule lives here rather than in `extract_literals`, which synthesis
/// shares and where the indented-code rule is load-bearing.
///
/// The cost, accepted deliberately: a fabricated diff line — `- removed this` —
/// is now missed, because a diff and a bullet share a prefix. Diffs in answers
/// are rare and four-space nested bullets are not. A flag is unaffected, since
/// `--dry-run` has no space after the dashes.
fn looks_like_a_list_item(lit: &str) -> bool {
    let t = lit.trim_start();
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        return rest.starts_with(' ');
    }
    let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
    digits.len() < t.len() && digits.starts_with(". ")
}

/// A literal escaped the way the sanitizer writes text, so it can be looked for
/// as the text a reader sees.
///
/// Only the three characters that carry structure. `"` and `'` are deliberately
/// left alone: the rendered answer has been through `ammonia`, whose serializer
/// escapes them in attribute values and not in text, so escaping them here would
/// make every literal containing a quote unmatchable. They are harmless in the
/// position this ever writes into — element content, never an attribute — and
/// `mark_unsupported` quotes its own attribute itself.
///
/// Local rather than a dependency because the requirement is not "escape HTML"
/// but "escape it exactly as the document already is": a general escaper that
/// also handled quotes would be correct in the abstract and wrong here.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wrap each unsupported literal in the rendered answer.
///
/// Operates on already-escaped HTML, and escapes the needle the same way before
/// searching, so a literal is matched as the text a reader sees. That is also
/// what makes it safe: the needle never re-enters the document as markup.
///
/// Only text is searched, never the inside of a tag. A literal that is also a
/// path would otherwise match inside an `href`, and a `<mark>` opened in the
/// middle of an attribute value is markup the sanitizer never saw.
///
/// Longest first, so a literal that contains a shorter one is marked whole
/// rather than being broken in half by the shorter match landing first.
pub fn mark_unsupported(html: &str, literals: &[String]) -> String {
    let mut needles: Vec<String> = literals
        .iter()
        .map(|l| escape_text(l))
        .filter(|n| !n.is_empty())
        .collect();
    // Length first, then value, so `dedup` — which only drops neighbours —
    // actually sees the duplicates.
    needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    needles.dedup();
    if needles.is_empty() {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(open) = rest.find('<') else {
            out.push_str(&mark_text(rest, &needles));
            return out;
        };
        out.push_str(&mark_text(&rest[..open], &needles));
        match rest[open..].find('>') {
            Some(close) => {
                out.push_str(&rest[open..open + close + 1]);
                rest = &rest[open + close + 1..];
            }
            // Unterminated `<`. Cannot happen in serialized output, and copying
            // the remainder through unchanged is the one response that can
            // neither drop text nor invent markup.
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
}

/// One left-to-right pass over a run of text between tags.
///
/// A pass per literal would let a later one match inside the `<mark>` an
/// earlier one just wrote, which is how a marker corrupts its own output. This
/// never looks at what it has already emitted.
fn mark_text(text: &str, needles: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match needles.iter().find(|n| text[i..].starts_with(n.as_str())) {
            Some(n) => {
                out.push_str(r#"<mark class="unsupported">"#);
                out.push_str(n);
                out.push_str("</mark>");
                i += n.len();
            }
            None => {
                let c = text[i..].chars().next().expect("i is on a char boundary");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fidelity thesis extended to generation: an answer cannot carry a
    /// literal the excerpts did not.
    #[test]
    fn a_command_in_no_excerpt_is_unsupported() {
        let excerpts = vec!["Run `systemctl restart engram` to apply it.".to_string()];
        let answer = "Run `systemctl restart engram`, then `rm -rf /var/lib/engram`.";
        let got = unsupported_literals(answer, &excerpts);
        assert!(
            got.iter().any(|l| l.contains("rm -rf /var/lib/engram")),
            "the invented command must be flagged: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.contains("systemctl restart engram")),
            "a command that is in an excerpt must not be: {got:?}"
        );
    }

    /// Nothing invented, nothing flagged — the common case, and the one that
    /// must not produce a badge on every answer.
    #[test]
    fn an_answer_drawn_entirely_from_its_excerpts_flags_nothing() {
        let excerpts = vec!["Set `fetch_max_bytes = 8388608` in config.toml.".to_string()];
        let answer = "Set `fetch_max_bytes = 8388608` in config.toml.";
        assert!(unsupported_literals(answer, &excerpts).is_empty());
    }

    /// Nothing was shown, so nothing can be checked against it. Flagging every
    /// literal in that case would badge exactly the answers the abstention
    /// paths already explain.
    #[test]
    fn an_answer_with_no_excerpts_at_all_is_not_checked() {
        assert!(unsupported_literals("Run `rm -rf /`.", &[]).is_empty());
    }

    /// A bullet list nested four spaces deep is markdown, not code, however
    /// `extract_literals` reads it. Flagging three sentences of ordinary prose
    /// on one answer is what teaches a reader to ignore the badge.
    #[test]
    fn a_bullet_list_nested_four_spaces_deep_is_formatting_not_invention() {
        let excerpts =
            vec!["The cliff drops weak excerpts; the budget drops what does not fit.".to_string()];
        let answer = "Two knobs:\n\n- retrieval\n    - the cliff drops weak excerpts\n    \
                      - the budget drops what does not fit\n";
        assert!(
            unsupported_literals(answer, &excerpts).is_empty(),
            "{:?}",
            unsupported_literals(answer, &excerpts)
        );
    }

    /// A numbered list is the same shape with a different marker.
    #[test]
    fn a_numbered_list_nested_four_spaces_deep_is_formatting_too() {
        let excerpts = vec!["Stop the service, then start it again.".to_string()];
        let answer = "Order:\n\n1. first\n    1. stop the service\n    2. start it again\n";
        assert!(unsupported_literals(answer, &excerpts).is_empty());
    }

    /// The case the list rule must not break: a flag has no space after its
    /// dashes, and an invented one in prose is exactly what is being looked
    /// for.
    #[test]
    fn an_invented_flag_is_still_caught_beside_the_list_rule() {
        let excerpts = vec!["Run `engram reindex` to rebuild.".to_string()];
        let answer = "Run it with --dry-run first.";
        assert_eq!(
            unsupported_literals(answer, &excerpts),
            vec!["--dry-run".to_string()]
        );
    }

    /// And the other half: an invented command indented as code is still
    /// caught. The rule drops bullets, not indented lines.
    #[test]
    fn an_invented_command_in_an_indented_block_survives_the_list_rule() {
        let excerpts = vec!["Unmount the device first.".to_string()];
        let answer = "Do this:\n\n    wipefs --all /dev/sdX\n";
        assert_eq!(
            unsupported_literals(answer, &excerpts),
            vec!["wipefs --all /dev/sdX".to_string()]
        );
    }

    /// Marking happens inside code fences too. A fabricated command is exactly
    /// the case this exists for, and exempting the place literals actually live
    /// would make the check decorative.
    #[test]
    fn marking_reaches_inside_a_code_block() {
        let html = "<pre><code>rm -rf /var/lib/engram</code></pre>";
        let marked = mark_unsupported(html, &["rm -rf /var/lib/engram".to_string()]);
        assert!(marked.contains(r#"<mark class="unsupported">"#), "{marked}");
    }

    /// The marker must never be able to inject markup: a literal is model
    /// output, and model output is untrusted.
    #[test]
    fn a_literal_containing_markup_cannot_break_out() {
        let html = "<p>&lt;script&gt;x&lt;/script&gt;</p>";
        let marked = mark_unsupported(html, &["<script>x</script>".to_string()]);
        assert!(
            !marked.contains("<script>"),
            "raw script tag leaked: {marked}"
        );
    }

    /// The escaped needle is what is looked for, so the marking actually lands
    /// on the escaped text rather than silently finding nothing.
    #[test]
    fn a_literal_containing_markup_is_still_marked_where_the_reader_sees_it() {
        let html = "<p>&lt;script&gt;x&lt;/script&gt;</p>";
        let marked = mark_unsupported(html, &["<script>x</script>".to_string()]);
        assert_eq!(
            marked,
            r#"<p><mark class="unsupported">&lt;script&gt;x&lt;/script&gt;</mark></p>"#
        );
    }

    /// A literal that is also a path must not be marked inside an attribute:
    /// a `<mark>` opened in the middle of an `href` is markup nothing
    /// sanitized.
    #[test]
    fn a_literal_inside_an_attribute_value_is_left_alone() {
        let html = r#"<p><a href="/docs/setup.md">setup</a> lives at /docs/setup.md</p>"#;
        let marked = mark_unsupported(html, &["/docs/setup.md".to_string()]);
        assert!(
            marked.contains(r#"<a href="/docs/setup.md">"#),
            "the attribute was rewritten: {marked}"
        );
        assert_eq!(
            marked.matches(r#"<mark class="unsupported">"#).count(),
            1,
            "only the visible text is marked: {marked}"
        );
    }

    /// A literal that starts where a shorter one does is marked whole. Two
    /// candidates at the same position is the only case ordering decides — a
    /// shorter one further along is swallowed by the longer match anyway — and
    /// the shorter winning would leave half a command marked, which reads as
    /// though the other half were supported.
    #[test]
    fn the_longest_literal_is_marked_whole() {
        let html = "<p>dd if=a.iso of=/dev/sdX</p>";
        let marked = mark_unsupported(
            html,
            &[
                "dd if=a.iso".to_string(),
                "dd if=a.iso of=/dev/sdX".to_string(),
            ],
        );
        assert_eq!(
            marked,
            r#"<p><mark class="unsupported">dd if=a.iso of=/dev/sdX</mark></p>"#
        );
    }

    /// A single pass, so a literal cannot match inside the marker a previous
    /// one wrote. `mark` is an ordinary word in a code span.
    #[test]
    fn a_literal_cannot_match_inside_the_marker_itself() {
        let html = "<p>set mark and unsupported</p>";
        let marked = mark_unsupported(html, &["mark".to_string(), "unsupported".to_string()]);
        assert_eq!(
            marked.matches("</mark>").count(),
            2,
            "one marker per occurrence, and none inside another: {marked}"
        );
    }

    /// Nothing to mark leaves the document byte-for-byte alone.
    #[test]
    fn an_answer_with_nothing_unsupported_is_returned_unchanged() {
        let html = "<p>ordinary prose</p>";
        assert_eq!(mark_unsupported(html, &[]), html);
    }

    /// The escaping has to match the renderer that actually produced the page,
    /// not HTML escaping in the abstract: `ammonia` leaves quotes raw in text,
    /// so escaping them here would make the literal unfindable.
    #[test]
    fn a_literal_containing_quotes_is_marked_in_really_rendered_html() {
        let answer = "Run `grep -r \"it's here\" .` to find it.";
        let html = crate::web::markdown::render(answer);
        let marked = mark_unsupported(&html, &["grep -r \"it's here\" .".to_string()]);
        assert!(
            marked.contains(r#"<mark class="unsupported">"#),
            "the literal was not found in the rendered HTML: {html}"
        );
    }

    /// Multi-byte text is walked by character, never by byte.
    #[test]
    fn a_literal_beside_multibyte_prose_does_not_panic() {
        let html = "<p>Die Größe ist /etc/fstab — prüfen.</p>";
        let marked = mark_unsupported(html, &["/etc/fstab".to_string()]);
        assert!(marked.contains("Größe"), "{marked}");
        assert!(marked.contains(r#"<mark class="unsupported">/etc/fstab</mark>"#));
    }
}
