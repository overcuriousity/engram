use super::budget::TokenCounter;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
    /// Leading lines of `text` that come from outside `start_line..=end_line` —
    /// the heading `flush` carries over, and nothing else. Anything measuring an
    /// offset within the window against the document has to skip them, or every
    /// line it reports is one too high.
    pub carry_lines: i64,
}

fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') && t.trim_start_matches('#').starts_with(' ')
}

/// Split `text` into windows that each fit `segment_tokens`.
///
/// Boundary preference is headings, then blank lines, then a hard cut. Each
/// window after the first repeats the most recent heading, so a procedure
/// split across windows still tells the model what it belongs to.
pub fn split_into_segments(
    text: &str,
    counter: &TokenCounter,
    segment_tokens: usize,
) -> Vec<Window> {
    if text.trim().is_empty() {
        return vec![];
    }
    if counter.count(text) <= segment_tokens {
        let lines = text.lines().count().max(1) as i64;
        return vec![Window {
            text: text.to_string(),
            start_line: 1,
            end_line: lines,
            carry_lines: 0,
        }];
    }

    // A single line over budget is cut before windowing, so every unit the
    // loop below places is guaranteed to fit in a window on its own. Without
    // this the loop can only ever emit one window for such a line, and that
    // window is however large the line was.
    //
    // Each piece keeps the number of the line it was cut from, because that is
    // the only number the document has: numbering the pieces instead shifted
    // every window after a cut line off the end of the source. Consecutive
    // windows can therefore report the same line, which is the honest answer —
    // a line number cannot address a unit smaller than a line, and it is why
    // the window's text is stored rather than re-derived from the range.
    let owned: Vec<(String, i64)> = text
        .lines()
        .enumerate()
        .flat_map(|(i, l)| {
            cut_long_line(l, segment_tokens, counter)
                .into_iter()
                .map(move |p| (p, i as i64 + 1))
        })
        .collect();
    let lines: Vec<(&str, i64)> = owned.iter().map(|(s, n)| (s.as_str(), *n)).collect();
    let mut windows: Vec<Window> = Vec::new();
    // Each buffered line with its document number, so a window can be cut
    // *inside* the buffer — at the last boundary before the line that
    // overflowed — and both halves still know where they came from.
    let mut buf: Vec<(&str, i64)> = Vec::new();
    let mut buf_tokens = 0usize;
    let mut last_heading: Option<String> = None;
    let mut carry: Option<String> = None;

    for (line, line_no) in lines.iter().copied() {
        let line_tokens = counter.count(line) + 1; // +1 for the newline
        let at_boundary = is_heading(line) || line.trim().is_empty();
        let overflows = !buf.is_empty() && buf_tokens + line_tokens > segment_tokens;
        let blank = line.trim().is_empty();

        if overflows && at_boundary {
            // Prefer to break at a heading or blank line. A blank line closes
            // the window it follows rather than opening the next one with an
            // empty first line; a heading opens the window it introduces.
            if blank {
                buf.push((line, line_no));
                flush_buf(&mut windows, &mut buf, &carry);
                carry = last_heading.clone();
                buf_tokens = 0;
            } else {
                flush_buf(&mut windows, &mut buf, &carry);
                last_heading = Some(line.to_string());
                carry = last_heading.clone();
                buf.push((line, line_no));
                buf_tokens = line_tokens;
            }
            continue;
        }

        if overflows {
            // The overflowing line is text, not a boundary. Cut at the last
            // boundary already in the buffer, if there is one: the window
            // before it respects the budget, and this line joins the one
            // after. Without this a paragraph that overflows rode along until
            // the next heading, and a 384-token passage came out at 500.
            let cut = buf
                .iter()
                .enumerate()
                .skip(1)
                .rev()
                .find_map(|(i, (l, _))| {
                    if is_heading(l) {
                        Some(i) // the heading opens the next window
                    } else if l.trim().is_empty() && i + 1 < buf.len() {
                        Some(i + 1) // the blank line closes this one
                    } else {
                        None
                    }
                });
            if let Some(i) = cut {
                let rest: Vec<(&str, i64)> = buf.split_off(i);
                // The heading the kept half continues under: the last one the
                // flushed half held, else whatever was carried before it.
                let flushed_heading = buf
                    .iter()
                    .rev()
                    .find(|(l, _)| is_heading(l))
                    .map(|(l, _)| l.to_string());
                flush_buf(&mut windows, &mut buf, &carry);
                if let Some(h) = flushed_heading {
                    carry = Some(h);
                } else if carry.is_none() {
                    carry = last_heading.clone();
                }
                buf = rest;
                buf_tokens = buf.iter().map(|(l, _)| counter.count(l) + 1).sum();
            } else if buf_tokens >= segment_tokens {
                // No boundary anywhere in a buffer already past the budget:
                // cut here rather than emit one huge window.
                flush_buf(&mut windows, &mut buf, &carry);
                carry = last_heading.clone();
                buf_tokens = 0;
            }
        }

        if is_heading(line) {
            last_heading = Some(line.to_string());
        }
        buf.push((line, line_no));
        buf_tokens += line_tokens;
    }
    flush_buf(&mut windows, &mut buf, &carry);
    windows
}

fn flush_buf(windows: &mut Vec<Window>, buf: &mut Vec<(&str, i64)>, carry: &Option<String>) {
    if buf.is_empty() {
        return;
    }
    let start = buf[0].1;
    let end = buf[buf.len() - 1].1;
    let mut lines: Vec<&str> = buf.iter().map(|(l, _)| *l).collect();
    flush(windows, &mut lines, start, end, carry);
    buf.clear();
}

fn flush(
    windows: &mut Vec<Window>,
    buf: &mut Vec<&str>,
    start: i64,
    end: i64,
    carry: &Option<String>,
) {
    if buf.is_empty() {
        return;
    }
    let body = buf.join("\n");
    let (text, carry_lines) = match carry {
        // Only prepend context when the window does not already open with a
        // heading of its own.
        Some(h) if !body.trim_start().starts_with('#') => (format!("{h}\n{body}"), 1),
        _ => (body, 0),
    };
    windows.push(Window {
        text,
        start_line: start,
        end_line: end,
        carry_lines,
    });
    buf.clear();
}

/// A line longer than a whole window has no boundary anywhere in it — a
/// minified blob, or text pasted with no newlines at all. Characters are the
/// last resort, and 3.5 per token is the same ratio the estimate uses, so a
/// part lands near the budget rather than far under it.
fn cut_long_line(line: &str, segment_tokens: usize, counter: &TokenCounter) -> Vec<String> {
    if counter.count(line) <= segment_tokens {
        return vec![line.to_string()];
    }
    let max_chars = (segment_tokens * 7 / 2).max(64);
    line.chars()
        .collect::<Vec<_>>()
        .chunks(max_chars)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

/// The exact lines of a stored window, one-based and inclusive.
///
/// Takes line numbers rather than a `Window` because windows live in the
/// database between job runs: the row may be stale, and clamping beats
/// panicking on data.
pub fn segment_text(text: &str, start_line: i64, end_line: i64) -> String {
    if start_line < 1 || end_line < start_line {
        return String::new();
    }
    text.lines()
        .skip((start_line - 1) as usize)
        .take((end_line - start_line + 1) as usize)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::budget::TokenCounter;

    #[test]
    fn short_text_is_a_single_window() {
        let w = split_into_segments("just a line", &TokenCounter, 1000);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, "just a line");
        assert_eq!(w[0].start_line, 1);
    }

    #[test]
    fn splits_on_headings_before_blank_lines() {
        let text = "## A\nalpha content here\n\n## B\nbeta content here\n\n## C\ngamma content";
        let w = split_into_segments(text, &TokenCounter, 12);
        assert!(w.len() >= 2);
        assert!(
            w[1].text.starts_with("## "),
            "a window must begin at a heading, got: {}",
            w[1].text
        );
    }

    #[test]
    fn windows_carry_one_heading_of_overlap() {
        let text = "## A\n".to_string() + &"alpha ".repeat(50) + "\n\n## B\n" + &"beta ".repeat(50);
        let w = split_into_segments(&text, &TokenCounter, 40);
        assert!(w.len() >= 2);
        for win in &w[1..] {
            assert!(
                win.text.contains("## A") || win.text.contains("## B"),
                "later windows must keep heading context, got: {}",
                win.text
            );
        }
    }

    #[test]
    fn line_numbers_are_one_based_and_contiguous() {
        let text = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let w = split_into_segments(&text, &TokenCounter, 30);
        assert_eq!(w[0].start_line, 1);
        assert_eq!(w.last().unwrap().end_line, 100);
        for pair in w.windows(2) {
            assert!(
                pair[1].start_line <= pair[0].end_line + 1,
                "gap between windows"
            );
        }
    }

    #[test]
    fn text_with_no_structure_still_splits_by_line_cap() {
        let text = (1..=500)
            .map(|i| format!("prose line number {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let w = split_into_segments(&text, &TokenCounter, 50);
        assert!(w.len() > 1, "unstructured text must still be windowed");
        for win in &w {
            assert!(
                TokenCounter.count(&win.text) <= 50 * 2,
                "window badly over budget: {} tokens",
                TokenCounter.count(&win.text)
            );
        }
    }

    #[test]
    fn a_corpus_with_no_newlines_is_still_windowed_within_budget() {
        // A paste from a PDF or a chat transcript is frequently one enormous
        // line. Returning it as a single window sent it to the model whole,
        // where it overflowed the context and retried with growing backoff
        // forever, because a job has no terminal state.
        let counter = TokenCounter;
        let blob = "word ".repeat(4000);
        assert!(!blob.contains('\n'), "the point is that there are no lines");

        let windows = split_into_segments(&blob, &counter, 256);

        assert!(windows.len() > 1, "got {} window(s)", windows.len());
        for w in &windows {
            assert!(
                counter.count(&w.text) <= 256,
                "window of {} tokens exceeds the budget",
                counter.count(&w.text)
            );
        }
        assert_eq!(
            windows.iter().map(|w| w.text.as_str()).collect::<String>(),
            blob,
            "cutting must not lose or duplicate text"
        );
    }

    #[test]
    fn a_cut_line_does_not_renumber_the_lines_after_it() {
        // Cutting an over-budget line used to expand the vector the loop
        // numbers from, so every window after the blob claimed lines the
        // document does not have — and those numbers are what an artifact's
        // corpus_span is clamped into and rendered from.
        let counter = TokenCounter;
        let mut lines = vec!["word ".repeat(2000)];
        for i in 1..=40 {
            lines.push(format!("ordinary line {i} with a few words on it"));
        }
        let text = lines.join("\n");
        let total = text.lines().count() as i64;

        let w = split_into_segments(&text, &counter, 256);

        assert!(w.len() > 2, "the fixture must produce several windows");
        for win in &w {
            assert!(
                win.start_line >= 1 && win.end_line <= total,
                "window {}-{} is outside a source of {total} lines",
                win.start_line,
                win.end_line
            );
        }
        assert_eq!(
            w.last().unwrap().end_line,
            total,
            "the last window must end at the last line of the source"
        );
    }

    #[test]
    fn every_line_of_the_source_survives_windowing() {
        // Windowing must lose nothing. Carried headings may duplicate, but no
        // original line may go missing.
        let text = (1..=300)
            .map(|i| {
                if i % 25 == 0 {
                    format!("## section {i}")
                } else {
                    format!("content line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let w = split_into_segments(&text, &TokenCounter, 60);
        let joined = w
            .iter()
            .map(|x| x.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for i in 1..=300 {
            let needle = if i % 25 == 0 {
                format!("## section {i}")
            } else {
                format!("content line {i}")
            };
            assert!(joined.contains(&needle), "windowing dropped: {needle}");
        }
    }

    #[test]
    fn window_text_returns_exactly_the_lines_a_window_claims() {
        let src = "one\ntwo\nthree\nfour\nfive";
        assert_eq!(segment_text(src, 2, 4), "two\nthree\nfour");
        // Out-of-range ends clamp rather than panic: the stored window is data,
        // and data can be stale.
        assert_eq!(segment_text(src, 4, 99), "four\nfive");
        assert_eq!(segment_text(src, 99, 120), "");
    }

    #[test]
    fn empty_input_produces_nothing() {
        assert!(split_into_segments("   \n  ", &TokenCounter, 100).is_empty());
    }

    #[test]
    fn a_paragraph_that_overflows_is_cut_at_the_boundary_before_it() {
        // Three sections of ~60 tokens each, budget 100. The third body line
        // overflows; the cut must fall at "## C" (before it), not at the next
        // heading after it — so no window exceeds the budget while a boundary
        // exists to cut at.
        let body = "word ".repeat(40); // ~57 tokens
        let text = format!("## A\n\n{body}\n\n## B\n\n{body}\n\n## C\n\n{body}\n\n## D\n\n{body}");
        let ws = split_into_segments(&text, &TokenCounter, 100);
        assert!(ws.len() >= 3, "{}", ws.len());
        for w in &ws {
            let own: String = w
                .text
                .lines()
                .skip(w.carry_lines as usize)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                TokenCounter.count(&own) <= 100,
                "window over budget ({} tokens): {:?}",
                TokenCounter.count(&own),
                own
            );
        }
        // Every heading opens its own window.
        assert!(
            ws.iter()
                .all(|w| w.text.lines().any(|l| l.starts_with("## ")))
        );
    }
}
