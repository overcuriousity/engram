//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A text source is answered by its lines; an image source by the model's
//! reading of the picture, labelled as such. A PDF source would be one more
//! arm of `slice`, its label reading `page 42`.

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

/// The lines of `source` around `span`, labelled for the pane. Without a span
/// the opening of the source is shown as context.
pub fn slice(source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice {
    // An image corpus's lines are the model's reading of the picture, and the
    // label says so: a span into a transcription is a claim about what the
    // model wrote, not about what the photo shows.
    let transcript = source.origin == crate::core::ingest::ORIGIN_IMAGE;
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
            label: if transcript {
                "transcription"
            } else {
                "corpus"
            }
            .into(),
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
        // Singular when the span is one line. "lines 576–576" is a range with
        // one thing in it, and a pane that says it has not checked what it is
        // about to claim.
        label: if span.start_line == span.end_line {
            format!(
                "{}line {}",
                if transcript { "transcription " } else { "" },
                span.start_line
            )
        } else {
            format!(
                "{}lines {}–{}",
                if transcript { "transcription " } else { "" },
                span.start_line,
                span.end_line
            )
        },
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
    async fn a_one_line_span_is_not_a_range() {
        // "lines 576–576" is a range with one thing in it, which reads as a
        // system that did not check what it was about to say.
        let src = a_corpus("l1\nl2\nl3").await;
        let slice = slice(
            &src,
            Some(&CorpusSpan {
                start_line: 2,
                end_line: 2,
            }),
            0,
        );
        assert_eq!(slice.label, "line 2");
    }

    #[tokio::test]
    async fn a_real_range_still_reads_as_one() {
        let src = a_corpus("l1\nl2\nl3").await;
        let slice = slice(
            &src,
            Some(&CorpusSpan {
                start_line: 1,
                end_line: 3,
            }),
            0,
        );
        assert_eq!(slice.label, "lines 1–3");
    }

    #[tokio::test]
    async fn the_slice_marks_the_span_and_carries_context_around_it() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6").await;
        let slice = slice(
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
        let slice = slice(&src, None, 1);
        assert_eq!(slice.label, "corpus");
        assert!(slice.lines.iter().all(|l| !l.in_span));
        assert_eq!(slice.lines.len(), 3);
    }

    #[tokio::test]
    async fn a_span_past_the_end_clamps_instead_of_panicking() {
        let src = a_corpus("l1\nl2").await;
        let slice = slice(
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
        assert_eq!(
            slice(
                &src,
                Some(&CorpusSpan {
                    start_line: 2,
                    end_line: 2
                }),
                0
            )
            .label,
            "transcription line 2"
        );
        assert_eq!(slice(&src, None, 0).label, "transcription");
    }
}
