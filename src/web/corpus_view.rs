//! How the right-hand pane gets at the text a chunk claims to come from.
//!
//! A text source is answered by its lines; an image source by the model's
//! reading of the picture, labelled as such; a PDF source by docling's
//! extraction of it, labelled as such. All three count lines. `page 42` would
//! be a nicer label for a PDF and a second coordinate system beside every
//! stored span, and the spec rejected it on those terms.

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
            label: written_down.unwrap_or("corpus").into(),
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
                written_down.map(|w| format!("{w} ")).unwrap_or_default(),
                span.start_line
            )
        } else {
            format!(
                "{}lines {}–{}",
                written_down.map(|w| format!("{w} ")).unwrap_or_default(),
                span.start_line,
                span.end_line
            )
        },
    }
}

/// One stretch of the source, and which artifacts claim to have been written
/// from it.
pub struct Band {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<CorpusLine>,
    /// Ids of the artifacts claiming these lines, in the order given. Empty
    /// means nothing claims them, which is what makes this a gap.
    pub artifact_ids: Vec<String>,
}

impl Band {
    /// Nothing was written from these lines. The whole point of banding: a
    /// passage the base cannot answer a question about, and one that can be
    /// told to read itself again.
    pub fn gap(&self) -> bool {
        self.artifact_ids.is_empty()
    }
}

/// Cut the source wherever the set of artifacts claiming it changes.
///
/// The corpus page's central arrangement: a band of source beside what came of
/// it. Overlaps become their own bands rather than being merged — where two
/// artifacts both claim a line, both are shown against it, which is the truth
/// and needs no tie-break.
///
/// This asks whether any artifact *claims* a line, which is a different
/// question from whether the line's wording survived into one. That second
/// question is `verify::content_coverage`, and it answers it as a fraction: a
/// faithfully rewritten line is a line whose wording did not survive and whose
/// content did, and marking it as missed made a hundred single-line warnings
/// out of one well-read document.
///
/// `highlight` is the `?from=&to=` deep link. Banding must not cost the page
/// its "open at these lines".
pub fn bands(
    raw_text: &str,
    spans: &[(String, CorpusSpan)],
    highlight: Option<(i64, i64)>,
) -> Vec<Band> {
    let mut out: Vec<Band> = Vec::new();

    for (i, text) in raw_text.lines().enumerate() {
        let number = i as i64 + 1;
        let mut ids: Vec<String> = spans
            .iter()
            .filter(|(_, s)| s.start_line <= number && number <= s.end_line)
            .map(|(id, _)| id.clone())
            .collect();

        // A blank line claims nothing and means nothing. Left to itself it
        // would cut a band in two, or open a red sliver between two paragraphs
        // with no content in it to have missed. It continues whatever band it
        // follows; at the head of a document there is nothing to continue, so
        // it starts one.
        if text.trim().is_empty()
            && let Some(last) = out.last()
        {
            ids = last.artifact_ids.clone();
        }

        let line = CorpusLine {
            number,
            text: text.to_string(),
            in_span: highlight.is_some_and(|(f, t)| number >= f && number <= t),
        };

        match out.last_mut() {
            Some(b) if b.artifact_ids == ids => {
                b.to = number;
                b.lines.push(line);
            }
            _ => out.push(Band {
                from: number,
                to: number,
                lines: vec![line],
                artifact_ids: ids,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::CorpusSpan;

    async fn a_corpus(raw: &str) -> Corpus {
        let s = crate::store::Store::memory().await.unwrap();
        s.insert_corpus(raw, "web", None).await.unwrap()
    }

    fn span(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan {
            start_line: a,
            end_line: b,
        }
    }

    /// `(from, to, ids)` for each band, which is what every case below asserts.
    fn shape(bs: &[Band]) -> Vec<(i64, i64, Vec<&str>)> {
        bs.iter()
            .map(|b| {
                (
                    b.from,
                    b.to,
                    b.artifact_ids.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn adjacent_spans_are_two_bands() {
        let out = bands(
            "a\nb\nc\nd",
            &[("x".into(), span(1, 2)), ("y".into(), span(3, 4))],
            None,
        );
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"]), (3, 4, vec!["y"])]);
    }

    #[test]
    fn an_overlap_is_its_own_band() {
        // Three bands, not a merge and not a tie-break: the middle really is
        // claimed by both, and saying so is the only honest arrangement.
        let out = bands(
            "a\nb\nc\nd\ne",
            &[("x".into(), span(1, 3)), ("y".into(), span(3, 5))],
            None,
        );
        assert_eq!(
            shape(&out),
            vec![(1, 2, vec!["x"]), (3, 3, vec!["x", "y"]), (4, 5, vec!["y"])]
        );
    }

    #[test]
    fn a_run_nothing_claims_is_a_gap_band() {
        let out = bands("a\nb\nc\nd", &[("x".into(), span(1, 2))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"]), (3, 4, vec![])]);
        assert!(!out[0].gap());
        assert!(out[1].gap());
    }

    #[test]
    fn a_gap_at_the_head_is_an_ordinary_band() {
        let out = bands("a\nb\nc", &[("x".into(), span(3, 3))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec![]), (3, 3, vec!["x"])]);
    }

    #[test]
    fn blank_lines_between_two_spans_join_the_band_before_them() {
        // A red sliver between two paragraphs would be noise with nothing to
        // re-read: there is no content there to have missed.
        let out = bands(
            "a\n\n\nb",
            &[("x".into(), span(1, 1)), ("y".into(), span(4, 4))],
            None,
        );
        assert_eq!(shape(&out), vec![(1, 3, vec!["x"]), (4, 4, vec!["y"])]);
    }

    #[test]
    fn a_corpus_nothing_was_written_from_is_one_gap() {
        let out = bands("a\nb\nc", &[], None);
        assert_eq!(shape(&out), vec![(1, 3, vec![])]);
    }

    #[test]
    fn one_artifact_over_the_whole_document_is_one_band() {
        let out = bands("a\nb\nc", &[("x".into(), span(1, 3))], None);
        assert_eq!(shape(&out), vec![(1, 3, vec!["x"])]);
    }

    #[test]
    fn a_span_past_the_end_does_not_invent_lines() {
        let out = bands("a\nb", &[("x".into(), span(1, 9))], None);
        assert_eq!(shape(&out), vec![(1, 2, vec!["x"])]);
        assert_eq!(out[0].lines.len(), 2);
    }

    #[test]
    fn the_highlight_marks_its_lines_wherever_they_fall() {
        // The `?from=&to=` deep link an artifact's "open at these lines" uses.
        let out = bands("a\nb\nc", &[("x".into(), span(1, 3))], Some((2, 3)));
        let marked: Vec<i64> = out[0]
            .lines
            .iter()
            .filter(|l| l.in_span)
            .map(|l| l.number)
            .collect();
        assert_eq!(marked, vec![2, 3]);
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
                    end_line: 3
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
