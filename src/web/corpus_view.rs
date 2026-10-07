//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A text source is answered by its lines; an image source by the model's
//! reading of the picture, labelled as such; a PDF source by docling's
//! extraction of it, labelled as such. All three count lines. `page 42` would
//! be a nicer label for a PDF and a second coordinate system beside every
//! stored span, and the spec rejected it on those terms.

pub use crate::core::coverage::{Band, CorpusLine, bands};
use crate::store::artifacts::CorpusSpan;
use crate::store::corpora::Corpus;

/// Lines shown without a span are context, not the claim itself.
const HEADLESS_PREVIEW_LINES: usize = 40;
/// `Default` is the empty slice, which is what a merged artifact has: it
/// belongs to no corpus, so there are no lines to show beside it and no range
/// to name. The detail pane renders its sources there instead.
#[derive(Default)]
pub struct CorpusSlice {
    pub lines: Vec<CorpusLine>,
    /// What to call this range in the UI: `lines 118–141`, or
    /// `extraction lines 118–141` where the lines are not the source's own.
    pub label: String,
}

/// The lines of `source` around `span`, labelled for the pane. Without a span
/// the opening of the source is shown as context.
///
/// One span through `slice_over`, which is where the work happens: there is one
/// definition of what a slice is and this is the way into it.
pub fn slice(source: &Corpus, span: Option<&CorpusSpan>, context: usize) -> CorpusSlice {
    match span {
        Some(sp) => slice_over(source, std::slice::from_ref(sp), context),
        None => slice_over(source, &[], context),
    }
}

/// The lines of `source` covering a run of spans, labelled for the pane.
///
/// `in_span` is true for a line inside *any* of the spans. What falls between
/// two of them is context, and is marked as such: claiming those lines were
/// read would be the one dishonesty this column must not commit.
///
/// An empty run is the headless case — no span, so the opening of the source
/// stands as context.
fn slice_over(source: &Corpus, spans: &[CorpusSpan], context: usize) -> CorpusSlice {
    // An image corpus's lines are the model's reading of the picture, and a
    // PDF's are docling's extraction of it. The label says so in both cases: a
    // span into either is a claim about what was written down, not about what
    // the source showed.
    let written_down = match source.origin.as_str() {
        crate::core::ingest::ORIGIN_IMAGE => Some("transcription"),
        crate::core::ingest::ORIGIN_PDF => Some("extraction"),
        _ => None,
    };
    let all: Vec<&str> = source.raw_text.lines().collect();
    let total = all.len() as i64;

    let (Some(first), Some(last)) = (
        spans.iter().map(|s| s.start_line).min(),
        spans.iter().map(|s| s.end_line).max(),
    ) else {
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
            label: written_down.unwrap_or("corpus").into(),
        };
    };

    let start = (first - context as i64).max(1);
    let end = (last + context as i64).min(total);
    let lines = (start..=end)
        .filter_map(|n| {
            all.get((n - 1) as usize).map(|t| CorpusLine {
                number: n,
                text: (*t).to_string(),
                in_span: spans.iter().any(|s| n >= s.start_line && n <= s.end_line),
            })
        })
        .collect();

    CorpusSlice {
        lines,
        // Singular when the run covers one line. "lines 576–576" is a range with
        // one thing in it, and a pane that says it has not checked what it is
        // about to claim.
        label: if first == last {
            format!(
                "{}line {}",
                written_down.map(|w| format!("{w} ")).unwrap_or_default(),
                first
            )
        } else {
            format!(
                "{}lines {}–{}",
                written_down.map(|w| format!("{w} ")).unwrap_or_default(),
                first,
                last
            )
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::CorpusSpan;

    fn span(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan {
            start_line: a,
            end_line: b,
            source: crate::store::artifacts::SpanSource::Located,
        }
    }

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
                source: crate::store::artifacts::SpanSource::Located,
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
                source: crate::store::artifacts::SpanSource::Located,
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
                source: crate::store::artifacts::SpanSource::Located,
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

    /// The pane appends the passages that follow, and the source column beside
    /// it has to grow with them. Recomputed over the whole run rather than
    /// appended: each slice carries context lines at both edges, so appending
    /// would print the lines between two adjacent passages twice.
    #[tokio::test]
    async fn a_run_of_spans_is_one_slice_with_no_line_printed_twice() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").await;
        // Adjacent passages: 2–3 and 4–5. With one line of context each, the
        // two single slices would both carry line 3 and line 4.
        let slice = slice_over(&src, &[span(2, 3), span(4, 5)], 1);

        let numbers: Vec<i64> = slice.lines.iter().map(|l| l.number).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6], "a line came back twice");
    }

    /// Every line the run was written from is the claim; the lines around it
    /// are context. A gap between two passages — a superseded row stepped over
    /// — is context too, and must not be marked as though something on screen
    /// was drawn from it.
    #[tokio::test]
    async fn a_run_marks_every_span_and_nothing_between_them() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").await;
        let slice = slice_over(&src, &[span(2, 2), span(5, 6)], 1);

        let marked: Vec<i64> = slice
            .lines
            .iter()
            .filter(|l| l.in_span)
            .map(|l| l.number)
            .collect();
        assert_eq!(marked, vec![2, 5, 6]);
    }

    /// The label names what is on screen. Over a run that is the union, and
    /// saying only the first passage's range would describe a column the reader
    /// can see is longer than that.
    #[tokio::test]
    async fn a_run_is_labelled_with_the_range_it_covers() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").await;
        let slice = slice_over(&src, &[span(2, 3), span(4, 5)], 1);
        assert_eq!(slice.label, "lines 2–5");
    }

    /// One span through the run-aware path is the single-passage pane, which is
    /// every pane before the reader has appended anything. It must not drift
    /// from what `slice` produces.
    #[tokio::test]
    async fn one_span_over_the_run_is_what_the_single_slice_already_was() {
        let src = a_corpus("l1\nl2\nl3\nl4\nl5\nl6").await;
        let one = slice(&src, Some(&span(3, 4)), 1);
        let run = slice_over(&src, &[span(3, 4)], 1);

        assert_eq!(run.label, one.label);
        assert_eq!(
            run.lines
                .iter()
                .map(|l| (l.number, l.in_span))
                .collect::<Vec<_>>(),
            one.lines
                .iter()
                .map(|l| (l.number, l.in_span))
                .collect::<Vec<_>>()
        );
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
                source: crate::store::artifacts::SpanSource::Located,
            }),
            2,
        );
        assert!(slice.lines.iter().all(|l| l.number <= 2));
    }

    #[tokio::test]
    async fn a_pdf_corpus_labels_its_lines_as_an_extraction() {
        // The lines belong to docling, not to the PDF's layout, and a span
        // into them is a claim about what was extracted. Same move as the
        // image arm's `transcription`, same reason.
        let s = crate::store::Store::memory().await.unwrap();
        let src = s
            .insert_attached_corpus(
                "h",
                crate::core::ingest::ORIGIN_PDF,
                None,
                None,
                &serde_json::json!({}),
                crate::store::corpora::Reading::EXTRACTION,
                &crate::store::attachments::NewFile {
                    kind: "pdf",
                    mime: "application/pdf",
                    filename: None,
                    bytes: b"%PDF-",
                    preview: b"",
                    width: None,
                    height: None,
                },
            )
            .await
            .unwrap()
            .into_corpus();
        s.set_read_text(&src.id, "a\nb\nc", vec![]).await.unwrap();
        let src = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(slice(&src, None, 0).label, "extraction");
        assert_eq!(
            slice(
                &src,
                Some(&CorpusSpan {
                    start_line: 2,
                    end_line: 3,
                    source: crate::store::artifacts::SpanSource::Located,
                }),
                0
            )
            .label,
            "extraction lines 2–3"
        );
    }

    #[tokio::test]
    async fn an_image_corpus_labels_its_lines_as_transcription() {
        let s = crate::store::Store::memory().await.unwrap();
        let src = s
            .insert_attached_corpus(
                "h",
                "image",
                None,
                None,
                &serde_json::json!({}),
                crate::store::corpora::Reading::VISION,
                &crate::store::attachments::NewFile {
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
        s.set_read_text(&src.id, "a\nb\nc", vec![]).await.unwrap();
        let src = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(
            slice(
                &src,
                Some(&CorpusSpan {
                    start_line: 2,
                    end_line: 2,
                    source: crate::store::artifacts::SpanSource::Located,
                }),
                0
            )
            .label,
            "transcription line 2"
        );
        assert_eq!(slice(&src, None, 0).label, "transcription");
    }
}
