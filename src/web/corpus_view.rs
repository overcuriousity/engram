//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A trait rather than a function because the answer depends on what the
//! source is. A text source is answered by `TextLines`; an image source by
//! `ImageTranscript`, whose lines are the model's reading of the picture. A
//! PDF source will implement the same trait — its label reads `page 42` and
//! its lines come from extracted text — and nothing in the pane needs to know
//! which implementation answered.

use crate::store::artifacts::CorpusSpan;
use crate::store::corpora::Corpus;

/// Lines shown without a span are context, not the claim itself.
const HEADLESS_PREVIEW_LINES: usize = 40;

pub struct CorpusLine {
    pub number: i64,
    pub text: String,
    /// Inside the chunk's span, as opposed to the context around it.
    pub in_span: bool,
}

/// `Default` is the empty slice, which is what a merged artifact has: it
/// belongs to no corpus, so there are no lines to show beside it and no range
/// to name. The detail pane renders its sources there instead.
#[derive(Default)]
pub struct CorpusSlice {
    pub lines: Vec<CorpusLine>,
    /// What to call this range in the UI: `lines 118–141`, later `page 42`.
    pub label: String,
}

pub trait CorpusView {
    fn slice(&self, source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice;
}

pub struct TextLines;

impl CorpusView for TextLines {
    fn slice(&self, source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice {
        let all: Vec<&str> = source.raw_text.lines().collect();
        let total = all.len() as i64;

        let Some(span) = span else {
            return CorpusSlice {
                lines: all
                    .iter()
                    .enumerate()
                    .take(HEADLESS_PREVIEW_LINES)
                    .map(|(i, t)| CorpusLine {
                        number: i as i64 + 1,
                        text: (*t).to_string(),
                        in_span: false,
                    })
                    .collect(),
                label: "corpus".into(),
            };
        };

        let start = (span.start_line - context as i64).max(1);
        let end = (span.end_line + context as i64).min(total);
        let lines = (start..=end)
            .filter_map(|n| {
                all.get((n - 1) as usize).map(|t| CorpusLine {
                    number: n,
                    text: (*t).to_string(),
                    in_span: n >= span.start_line && n <= span.end_line,
                })
            })
            .collect();

        CorpusSlice {
            lines,
            label: format!("lines {}–{}", span.start_line, span.end_line),
        }
    }
}

/// An image corpus: the lines are the model's reading of the picture, and the
/// label says so, because a span into a transcription is a claim about what
/// the model wrote, not about what the photo shows.
pub struct ImageTranscript;

impl CorpusView for ImageTranscript {
    fn slice(&self, source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice {
        let mut s = TextLines.slice(source, span, context);
        s.label = match span {
            Some(sp) => format!("transcription lines {}–{}", sp.start_line, sp.end_line),
            None => "transcription".into(),
        };
        s
    }
}

/// The view for a source. This is where a PDF source will branch too.
pub fn for_corpus(source: &Corpus) -> Box<dyn CorpusView> {
    if source.origin == crate::core::ingest::ORIGIN_IMAGE {
        Box::new(ImageTranscript)
    } else {
        Box::new(TextLines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::CorpusSpan;

    async fn a_corpus(raw: &str) -> Corpus {
        let s = crate::store::Store::memory().await.unwrap();
        s.insert_corpus(raw, "web", None).await.unwrap()
    }

    #[tokio::test]
    async fn the_slice_marks_the_span_and_carries_context_around_it() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6").await;
        let slice = TextLines.slice(
            &src,
            Some(&CorpusSpan {
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
        let src = a_corpus("l1\nl2\nl3").await;
        let slice = TextLines.slice(&src, None, 1);
        assert_eq!(slice.label, "corpus");
        assert!(slice.lines.iter().all(|l| !l.in_span));
        assert_eq!(slice.lines.len(), 3);
    }

    #[tokio::test]
    async fn a_span_past_the_end_clamps_instead_of_panicking() {
        let src = a_corpus("l1\nl2").await;
        let slice = TextLines.slice(
            &src,
            Some(&CorpusSpan {
                start_line: 5,
                end_line: 9,
            }),
            2,
        );
        assert!(slice.lines.iter().all(|l| l.number <= 2));
    }

    #[tokio::test]
    async fn an_image_corpus_labels_its_lines_as_transcription() {
        let s = crate::store::Store::memory().await.unwrap();
        let src = s
            .insert_image_corpus(
                "h",
                "image",
                None,
                &serde_json::json!({}),
                &crate::store::attachments::NewImage {
                    kind: "image",
                    mime: "image/png",
                    filename: None,
                    bytes: b"orig",
                    preview: b"prev",
                    width: Some(1),
                    height: Some(1),
                },
            )
            .await
            .unwrap()
            .into_corpus();
        s.set_described_text(&src.id, "a\nb\nc", vec![])
            .await
            .unwrap();
        let src = s.get_corpus(&src.id).await.unwrap();
        let view = for_corpus(&src);
        assert_eq!(
            view.slice(
                &src,
                Some(&CorpusSpan {
                    start_line: 2,
                    end_line: 2
                }),
                0
            )
            .label,
            "transcription lines 2–2"
        );
        assert_eq!(view.slice(&src, None, 0).label, "transcription");
    }
}
