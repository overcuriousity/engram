//! Windowing via `text-splitter`, sized by [`TokenCounter`], translated back to 1-based inclusive
//! source lines, since spans are addresses the lineage and corpus views dereference.

use super::budget::TokenCounter;
use text_splitter::{ChunkConfig, MarkdownSplitter};

impl text_splitter::ChunkSizer for TokenCounter {
    fn size(&self, chunk: &str) -> usize {
        self.count(chunk)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub text: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// Line ranges partition the text and each window's `text` is exactly those lines: a window is the slice between
/// boundaries snapped to line starts. A hard cut inside one long line is the exception: both windows name it.
pub fn split_into_segments(text: &str, counter: &TokenCounter, budget: usize) -> Vec<Window> {
    if text.trim().is_empty() {
        return vec![];
    }
    let splitter = MarkdownSplitter::new(ChunkConfig::new(budget.max(1)).with_sizer(counter));
    let offsets: Vec<usize> = splitter.chunk_indices(text).map(|(off, _)| off).collect();
    if offsets.is_empty() {
        return vec![];
    }
    let line_of = |off: usize| text[..off].bytes().filter(|b| *b == b'\n').count() as i64 + 1;
    let line_start = |off: usize| text[..off].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let total_lines = text.lines().count().max(1) as i64;
    // One decision per boundary, read by both neighbours, or the straddled line's head belongs to no window.
    let froms: Vec<usize> =
        offsets
            .iter()
            .enumerate()
            .fold(Vec::with_capacity(offsets.len()), |mut acc, (i, off)| {
                // The first window claims the head of the document, whatever the splitter trimmed.
                let from = if i == 0 {
                    0
                } else {
                    let snapped = line_start(*off);
                    // Compare against the predecessor's content start, not its window start: they differ for window 0
                    // when leading blank lines were trimmed, which would collapse it to whitespace.
                    let prev = acc[i - 1].max(line_start(offsets[i - 1]));
                    if snapped > prev { snapped } else { *off }
                };
                acc.push(from);
                acc
            });
    let mut out = Vec::with_capacity(offsets.len());
    for i in 0..offsets.len() {
        let from = froms[i];
        let start = line_of(from);
        let (slice, end) = match offsets.get(i + 1) {
            Some(next) => {
                let to = froms[i + 1];
                // Unsnapped means a hard cut inside one long line; both windows then name it.
                if to == line_start(*next) {
                    (&text[from..to], line_of(*next) - 1)
                } else {
                    (&text[from..to], line_of(*next).max(start))
                }
            }
            None => (&text[from..], total_lines.max(start)),
        };
        out.push(Window {
            text: slice.to_string(),
            start_line: start,
            end_line: end,
        });
    }
    out
}

/// Takes line numbers because the row may be stale; clamping beats panicking on data.
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
        let w = split_into_segments("just a line", &TokenCounter::default(), 1000);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].text, "just a line");
        assert_eq!((w[0].start_line, w[0].end_line), (1, 1));
    }

    #[test]
    fn windows_partition_the_line_range_without_gaps() {
        let paras: Vec<String> = (0..6)
            .map(|i| format!("paragraph {i} words ").repeat(10))
            .collect();
        let text = paras.join("\n\n");
        let w = split_into_segments(&text, &TokenCounter::default(), 60);
        assert!(w.len() > 1, "{}", w.len());
        assert_eq!(w[0].start_line, 1);
        assert_eq!(w.last().unwrap().end_line, text.lines().count() as i64);
        for pair in w.windows(2) {
            assert_eq!(pair[0].end_line + 1, pair[1].start_line, "spans must abut");
        }
    }

    #[test]
    fn leading_blank_lines_do_not_collapse_the_first_window() {
        let counter = TokenCounter::default();
        for lead in ["\n\n\n\n", "\n", "   \n\n\t\n"] {
            let text = format!("{lead}{}", "line of words here. ".repeat(500));
            for budget in [64, 128, 256] {
                let w = split_into_segments(&text, &counter, budget);
                for win in &w {
                    // Splitter overshoot is a token or two; twice the budget means a neighbour's chunk.
                    assert!(
                        counter.count(&win.text) <= budget + budget / 2,
                        "lead {lead:?} budget {budget}: a window of {} tokens",
                        counter.count(&win.text)
                    );
                }
                let joined: String = w.iter().map(|x| x.text.as_str()).collect();
                assert_eq!(joined, text, "lead {lead:?} budget {budget}");
            }
        }
    }

    #[test]
    fn a_straddled_line_reaches_the_window_that_claims_it() {
        // The splitter trims markers off chunk starts, so the boundary lands mid-line.
        let text = format!(
            "# Notes\n\n{}\n\n> a quoted aside.\n\n{}\n\n    indented_code(arg)\n\n{}\n",
            "first paragraph words. ".repeat(24),
            "second paragraph words. ".repeat(24),
            "third paragraph words. ".repeat(24)
        );
        for budget in [40, 60, 80, 120] {
            let w = split_into_segments(&text, &TokenCounter::default(), budget);
            for (i, win) in w.iter().enumerate() {
                // Only a hard cut in one long line may overlap; everywhere else ranges abut.
                let cut = i > 0 && w[i - 1].end_line >= win.start_line
                    || w.get(i + 1).is_some_and(|n| win.end_line >= n.start_line);
                if cut {
                    continue;
                }
                // Through `lines()`: a window carries its trailing separator, a line range cannot.
                assert_eq!(
                    segment_text(&text, win.start_line, win.end_line),
                    win.text.lines().collect::<Vec<_>>().join("\n"),
                    "budget {budget}, lines {}-{}",
                    win.start_line,
                    win.end_line
                );
            }
        }
    }

    #[test]
    fn markdown_headings_prefer_to_open_a_window() {
        let text = format!(
            "## One\n{}\n## Two\n{}",
            "alpha words ".repeat(30),
            "beta words ".repeat(30)
        );
        let w = split_into_segments(&text, &TokenCounter::default(), 60);
        assert!(
            w.iter().any(|w| w.text.trim_start().starts_with("## Two")),
            "{:?}",
            w.iter()
                .map(|w| w.text.lines().next().unwrap_or(""))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn text_with_no_structure_still_splits_within_budget() {
        let text = "word ".repeat(600);
        let w = split_into_segments(&text, &TokenCounter::default(), 50);
        assert!(w.len() > 1);
        for win in &w {
            assert!(
                TokenCounter::default().count(&win.text) <= 50 * 2,
                "window far over budget: {}",
                TokenCounter::default().count(&win.text)
            );
        }
    }

    #[test]
    fn a_corpus_with_no_newlines_is_still_windowed_within_budget() {
        let text = "x".repeat(4000);
        let w = split_into_segments(&text, &TokenCounter::default(), 100);
        assert!(!w.is_empty());
        for win in &w {
            assert!(TokenCounter::default().count(&win.text) <= 100 * 2);
            assert_eq!(
                (win.start_line, win.end_line),
                (1, 1),
                "one line is all there is"
            );
        }
    }

    #[test]
    fn every_content_line_survives_windowing() {
        let text = format!(
            "# T\n\nalpha {}\n\nbeta {}\n\n## S\ngamma",
            "one ".repeat(40),
            "two ".repeat(40)
        );
        let w = split_into_segments(&text, &TokenCounter::default(), 60);
        let joined: String = w
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            assert!(joined.contains(line.trim()), "lost: {line:?}");
        }
    }

    #[test]
    fn segment_text_returns_exactly_the_lines_a_window_claims() {
        let text = "l1\nl2\nl3\nl4";
        assert_eq!(segment_text(text, 2, 3), "l2\nl3");
        assert_eq!(segment_text(text, 0, 3), "");
        assert_eq!(segment_text(text, 3, 2), "");
        assert_eq!(segment_text(text, 3, 99), "l3\nl4");
    }

    #[test]
    fn empty_input_produces_nothing() {
        assert!(split_into_segments("  \n \n", &TokenCounter::default(), 100).is_empty());
    }
}
