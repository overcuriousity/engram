//! Windowing, by the industry's splitter instead of a hand-rolled one.
//!
//! `text-splitter` does the boundary work — markdown structure first, then
//! paragraphs, sentences, and a hard cut as the last resort — sized by the
//! same [`TokenCounter`] every other budget in the system uses. What this
//! module owns is the translation back to engram's coordinates: a window is
//! addressed by 1-based, inclusive source lines, because spans are addresses
//! the lineage and corpus views dereference against the stored text.
//!
//! The old splitter carried the governing heading into continuation windows.
//! That retired with it: the crate's structural boundaries replace the need,
//! and the synthesis prompt's context blocks already carry the document
//! opening for reference resolution.

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

/// Split `text` into windows that each fit `budget` counter-tokens.
///
/// The returned line ranges partition `1..=lines(text)` with no gaps, and —
/// the part that took two bugs to get right — each window's `text` is exactly
/// the source those lines hold. A window is the slice *between* two splitter
/// boundaries, not the chunk the splitter hands back: the crate trims
/// separators off a chunk, and a trimmed body under an untrimmed range is a
/// window whose line *k* is not `start_line + k - 1`, which is the one thing
/// everything downstream assumes. `"\n\n# Notes\nalpha"` used to give the first
/// window `start_line: 1` over a body beginning at `# Notes`, so the passage
/// holding the heading was addressed two lines above itself — and leading
/// blank lines are routine out of HTML and PDF extraction, which nothing trims
/// at ingest.
///
/// Boundaries are snapped back to the start of their line for the same reason.
/// `text-splitter` descends to sentence level, so a chunk can end mid-paragraph
/// and mid-line; the window then ends at the line before, and the whole of the
/// straddled line goes to the successor that already claims it. The one case
/// that cannot be line-aligned is a hard cut *inside* a single long line: there
/// the fragments are kept as they are, both windows name that one line, and
/// `split_passages`' clamp is what keeps the spans inside the document. Line
/// alignment there would hand each fragment's window the whole line and put the
/// same text in the base as many times as it was cut.
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
    // The first byte of the line `off` falls in.
    let line_start = |off: usize| text[..off].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let total_lines = text.lines().count().max(1) as i64;
    let mut out = Vec::with_capacity(offsets.len());
    for (i, off) in offsets.iter().enumerate() {
        // The first window claims the head of the document, whatever the
        // splitter trimmed off it — and now carries it, so the claim is true.
        let from = if i == 0 { 0 } else { *off };
        let start = line_of(from);
        let (slice, end) = match offsets.get(i + 1) {
            Some(next) => {
                let snapped = line_start(*next);
                if snapped > from {
                    (&text[from..snapped], line_of(*next) - 1)
                } else {
                    (&text[from..*next], line_of(*next).max(start))
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
