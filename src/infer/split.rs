use super::budget::TokenCounter;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
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
        }];
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut windows: Vec<Window> = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut buf_tokens = 0usize;
    let mut start_line = 1i64;
    let mut last_heading: Option<String> = None;
    let mut carry: Option<String> = None;

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx as i64 + 1;
        let line_tokens = counter.count(line) + 1; // +1 for the newline
        let at_boundary = is_heading(line) || line.trim().is_empty();

        // Flush when the line would overflow the window. Prefer to break at a
        // heading or blank line; if the buffer has grown well past the budget
        // with no boundary in sight, cut anyway rather than emit one huge window.
        let overflows = !buf.is_empty() && buf_tokens + line_tokens > segment_tokens;
        let blank = line.trim().is_empty();

        if overflows && (at_boundary || buf_tokens >= segment_tokens) {
            if blank {
                // A blank line separates; it closes the window it follows
                // rather than opening the next one with an empty first line.
                buf.push(line);
                flush(&mut windows, &mut buf, start_line, line_no, &carry);
                start_line = line_no + 1;
            } else {
                // A heading opens the window it introduces.
                flush(&mut windows, &mut buf, start_line, line_no - 1, &carry);
                start_line = line_no;
                if is_heading(line) {
                    last_heading = Some(line.to_string());
                }
                buf.push(line);
            }
            buf_tokens = if blank { 0 } else { line_tokens };
            carry = last_heading.clone();
            continue;
        }

        if is_heading(line) {
            last_heading = Some(line.to_string());
        }
        buf.push(line);
        buf_tokens += line_tokens;
    }
    flush(
        &mut windows,
        &mut buf,
        start_line,
        lines.len() as i64,
        &carry,
    );
    windows
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
    let text = match carry {
        // Only prepend context when the window does not already open with a
        // heading of its own.
        Some(h) if !body.trim_start().starts_with('#') => format!("{h}\n{body}"),
        _ => body,
    };
    windows.push(Window {
        text,
        start_line: start,
        end_line: end,
    });
    buf.clear();
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
        let w = split_into_segments("just a line", &TokenCounter::Estimate, 1000);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, "just a line");
        assert_eq!(w[0].start_line, 1);
    }

    #[test]
    fn splits_on_headings_before_blank_lines() {
        let text = "## A\nalpha content here\n\n## B\nbeta content here\n\n## C\ngamma content";
        let w = split_into_segments(text, &TokenCounter::Estimate, 12);
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
        let w = split_into_segments(&text, &TokenCounter::Estimate, 40);
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
        let w = split_into_segments(&text, &TokenCounter::Estimate, 30);
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
        let w = split_into_segments(&text, &TokenCounter::Estimate, 50);
        assert!(w.len() > 1, "unstructured text must still be windowed");
        for win in &w {
            assert!(
                TokenCounter::Estimate.count(&win.text) <= 50 * 2,
                "window badly over budget: {} tokens",
                TokenCounter::Estimate.count(&win.text)
            );
        }
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
        let w = split_into_segments(&text, &TokenCounter::Estimate, 60);
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
        assert!(split_into_segments("   \n  ", &TokenCounter::Estimate, 100).is_empty());
    }
}
