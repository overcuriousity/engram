//! Which lines of a capture any artifact claims, and which nothing was written from.
//!
//! Pure over the text and the spans, so the corpus page and the reconcile
//! sweep cut a document in exactly the same places.

use crate::store::artifacts::CorpusSpan;

pub struct CorpusLine {
    pub number: i64,
    pub text: String,
    /// Inside the chunk's span, as opposed to the context around it.
    pub in_span: bool,
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

    fn span(a: i64, b: i64) -> CorpusSpan {
        CorpusSpan {
            start_line: a,
            end_line: b,
            source: crate::store::artifacts::SpanSource::Located,
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
}
