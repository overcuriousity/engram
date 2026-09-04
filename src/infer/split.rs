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
    // Where each window begins, decided once and read by both sides of every
    // boundary. It has to be one decision: a window ended at `line_start(next)`
    // while its successor began at the raw `next`, so the bytes between the two
    // — the head of the straddled line, which is the whole reason the boundary
    // is snapped back — belonged to no window at all. A blockquote arrived
    // without its `> `, an indented code block without its four spaces, and
    // the successor's `start_line` named a line whose first byte it did not
    // hold, so `segment_text(start_line, end_line)` no longer reproduced
    // `Window::text`. The invariant this module's doc states held only for the
    // first window.
    let froms: Vec<usize> =
        offsets
            .iter()
            .enumerate()
            .fold(Vec::with_capacity(offsets.len()), |mut acc, (i, off)| {
                // The first window claims the head of the document, whatever the
                // splitter trimmed off it — and now carries it, so the claim is
                // true.
                let from = if i == 0 {
                    0
                } else {
                    let snapped = line_start(*off);
                    // A hard cut inside one long line cannot be snapped: both
                    // windows then name that line and take the fragments as they
                    // are. See the doc above.
                    //
                    // Compared against where the predecessor's own *content*
                    // begins, not against where its window begins. The two are
                    // the same everywhere but the first window, which claims
                    // the head of the document from byte zero however much the
                    // splitter trimmed off it — so with leading blank lines
                    // `acc[0]` was 0 while the first chunk started on line 5,
                    // and snapping the second boundary back to that same line 5
                    // cleared a bar that was standing in the wrong place. Window
                    // zero collapsed to the whitespace and window one carried
                    // two chunks: `"\n\n\n\n"` before a long line came back as
                    // one token and then 513 against a budget of 256, twice the
                    // input the context reservation is sized for, and a
                    // synthesis call spent on four newlines. Leading blanks are
                    // routine out of HTML and PDF extraction — see the note at
                    // the top of this function, which is about the same four
                    // characters.
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
                // Snapped: the successor opens at a line start, so this window
                // stops at the line before it. Unsnapped — the one case that
                // cannot be aligned — is a hard cut inside a single long line,
                // and then both windows name that line.
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

    /// Leading blank lines used to buy the document one wasted window and its
    /// successor two chunks' worth of text.
    ///
    /// The first window claims the head of the document from byte zero, so its
    /// recorded start is not where the splitter's first chunk begins. The
    /// snapping guard compared the second boundary against that zero, found it
    /// clear, and snapped the boundary onto the very line the first chunk
    /// opens on — leaving window zero holding nothing but the blank lines and
    /// window one holding both chunks. Four newlines out of a PDF extraction
    /// are enough, and what came back was a window at twice the budget: past
    /// what the context reservation is sized for, with a synthesis call spent
    /// on the whitespace beside it.
    #[test]
    fn leading_blank_lines_do_not_collapse_the_first_window() {
        let counter = TokenCounter::default();
        for lead in ["\n\n\n\n", "\n", "   \n\n\t\n"] {
            let text = format!("{lead}{}", "line of words here. ".repeat(500));
            for budget in [64, 128, 256] {
                let w = split_into_segments(&text, &counter, budget);
                for win in &w {
                    // The splitter's own overshoot is a token or two; twice the
                    // budget is a window carrying a neighbour's chunk.
                    assert!(
                        counter.count(&win.text) <= budget + budget / 2,
                        "lead {lead:?} budget {budget}: a window of {} tokens",
                        counter.count(&win.text)
                    );
                }
                // And the text still reconstructs, which is what the snapping
                // is for in the first place.
                let joined: String = w.iter().map(|x| x.text.as_str()).collect();
                assert_eq!(joined, text, "lead {lead:?} budget {budget}");
            }
        }
    }

    /// The invariant the module doc states, asserted where it used to break:
    /// a window's text is exactly the source its line range holds.
    ///
    /// The boundary the crate hands back can land mid-line, and it is snapped
    /// to the start of that line — but only the predecessor was snapped, so the
    /// head of the straddled line fell into the gap between the two windows.
    /// Markdown is where it shows: a blockquote arrived without its `> `, an
    /// indented code block without its indentation, and the model was shown
    /// text that is not the text that was stored.
    #[test]
    fn a_straddled_line_reaches_the_window_that_claims_it() {
        // Short structural lines between long paragraphs: the splitter opens a
        // chunk on each, and trims the marker off the front of it, so the
        // boundary lands mid-line with the whole line still to come.
        let text = format!(
            "# Notes\n\n{}\n\n> a quoted aside.\n\n{}\n\n    indented_code(arg)\n\n{}\n",
            "first paragraph words. ".repeat(24),
            "second paragraph words. ".repeat(24),
            "third paragraph words. ".repeat(24)
        );
        for budget in [40, 60, 80, 120] {
            let w = split_into_segments(&text, &TokenCounter::default(), budget);
            for (i, win) in w.iter().enumerate() {
                // The one documented exception: a hard cut inside a single
                // long line leaves two windows naming that line, and neither
                // holds the whole of it. Everywhere else the ranges abut, and
                // an overlap is exactly how that case announces itself.
                let cut = i > 0 && w[i - 1].end_line >= win.start_line
                    || w.get(i + 1).is_some_and(|n| win.end_line >= n.start_line);
                if cut {
                    continue;
                }
                // Both sides through `lines()`, because a window carries the
                // separator that ends it and a line range cannot; what is
                // being asserted is the lines, which is what the span
                // addresses.
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
