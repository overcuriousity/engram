//! Which lines of a capture any artifact claims, and which nothing was written from.
//! Pure over text and spans, so the corpus page and the reconcile sweep cut identically.

use crate::store::artifacts::CorpusSpan;

/// Shared by the queue row and the reconcile sweep; unmeasured (`None`) is not low.
pub fn low_coverage(coverage: Option<f64>) -> bool {
    coverage.is_some_and(|c| c < crate::infer::verify::LOW_COVERAGE)
}

pub struct CorpusLine {
    pub number: i64,
    pub text: String,
    pub in_span: bool,
}

/// A stretch of source lines and the artifacts claiming them.
pub struct Band {
    pub from: i64,
    pub to: i64,
    pub lines: Vec<CorpusLine>,
    /// Empty means nothing claims these lines: a gap.
    pub artifact_ids: Vec<String>,
}

impl Band {
    pub fn gap(&self) -> bool {
        self.artifact_ids.is_empty()
    }
}

/// Cut the source wherever the set of claiming artifacts changes; overlaps become their own bands.
/// Claims, not wording: whether a line's wording survived is `verify::content_coverage`.
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

        // A blank line continues the band it follows, so it never splits one or opens an empty gap.
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
