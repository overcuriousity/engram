//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A trait rather than a function because the answer depends on what the
//! source is. Today every source is raw text and `TextLines` answers all of
//! them. A PDF source will implement the same trait — its label reads
//! `page 42` and its lines come from extracted text — and nothing in the pane
//! needs to know which implementation answered.

use crate::store::chunks::SourceSpan;
use crate::store::sources::Source;

/// Lines shown without a span are context, not the claim itself.
const HEADLESS_PREVIEW_LINES: usize = 40;

pub struct SourceLine {
    pub number: i64,
    pub text: String,
    /// Inside the chunk's span, as opposed to the context around it.
    pub in_span: bool,
}

pub struct SourceSlice {
    pub lines: Vec<SourceLine>,
    /// What to call this range in the UI: `lines 118–141`, later `page 42`.
    pub label: String,
}

pub trait SourceView {
    fn slice(&self, source: &Source, span: Option<&SourceSpan>, context: usize) -> SourceSlice;
}

pub struct TextLines;

impl SourceView for TextLines {
    fn slice(&self, source: &Source, span: Option<&SourceSpan>, context: usize) -> SourceSlice {
        let all: Vec<&str> = source.raw_text.lines().collect();
        let total = all.len() as i64;

        let Some(span) = span else {
            return SourceSlice {
                lines: all
                    .iter()
                    .enumerate()
                    .take(HEADLESS_PREVIEW_LINES)
                    .map(|(i, t)| SourceLine {
                        number: i as i64 + 1,
                        text: (*t).to_string(),
                        in_span: false,
                    })
                    .collect(),
                label: "source".into(),
            };
        };

        let start = (span.start_line - context as i64).max(1);
        let end = (span.end_line + context as i64).min(total);
        let lines = (start..=end)
            .filter_map(|n| {
                all.get((n - 1) as usize).map(|t| SourceLine {
                    number: n,
                    text: (*t).to_string(),
                    in_span: n >= span.start_line && n <= span.end_line,
                })
            })
            .collect();

        SourceSlice {
            lines,
            label: format!("lines {}–{}", span.start_line, span.end_line),
        }
    }
}

/// The view for a source. One implementation today; this is where a PDF source
/// will branch.
pub fn for_source(_source: &Source) -> Box<dyn SourceView> {
    Box::new(TextLines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::chunks::SourceSpan;

    async fn a_source(raw: &str) -> Source {
        let s = crate::store::Store::memory().await.unwrap();
        s.insert_source(raw, "web", None).await.unwrap()
    }

    #[tokio::test]
    async fn the_slice_marks_the_span_and_carries_context_around_it() {
        let src = a_source("l1\nl2\nl3\nl4\nl5\nl6").await;
        let slice = TextLines.slice(
            &src,
            Some(&SourceSpan {
                start_line: 3,
                end_line: 4,
            }),
            1,
        );

        assert_eq!(slice.label, "lines 3–4");
        assert_eq!(slice.lines.first().unwrap().number, 2);
        assert_eq!(slice.lines.last().unwrap().number, 5);
        let marked: Vec<i64> = slice
            .lines
            .iter()
            .filter(|l| l.in_span)
            .map(|l| l.number)
            .collect();
        assert_eq!(marked, vec![3, 4]);
    }

    #[tokio::test]
    async fn a_chunk_without_a_span_gets_the_head_of_the_source() {
        let src = a_source("l1\nl2\nl3").await;
        let slice = TextLines.slice(&src, None, 1);
        assert_eq!(slice.label, "source");
        assert!(slice.lines.iter().all(|l| !l.in_span));
        assert_eq!(slice.lines.len(), 3);
    }

    #[tokio::test]
    async fn a_span_past_the_end_clamps_instead_of_panicking() {
        let src = a_source("l1\nl2").await;
        let slice = TextLines.slice(
            &src,
            Some(&SourceSpan {
                start_line: 5,
                end_line: 9,
            }),
            2,
        );
        assert!(slice.lines.iter().all(|l| l.number <= 2));
    }
}
