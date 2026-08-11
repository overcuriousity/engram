use crate::store::artifacts::CorpusSpan;
use crate::store::corpora::Corpus;

const HEADLESS_PREVIEW_LINES: usize = 40;

pub struct CorpusLine {
    pub number: i64,
    pub text: String,
    pub in_span: bool,
}

pub struct CorpusSlice {
    pub lines: Vec<CorpusLine>,
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

pub fn for_corpus(_source: &Corpus) -> Box<dyn CorpusView> {
    Box::new(TextLines)
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
}
