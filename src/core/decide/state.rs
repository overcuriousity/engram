use crate::infer::budget::TokenCounter;
use crate::infer::decide::{Kind, Question};

pub const BEARS_KEY: &str = "bears";
pub const NO_INFORMATION: &str = "No information.";

pub fn bears_question(q: &str) -> Question {
    Question {
        key: BEARS_KEY.into(),
        kind: Kind::Noul,
        instructions: format!(
            "Does this excerpt contain information that helps answer the question: «{q}»?"
        ),
    }
}

fn situation_block(s: &str) -> String {
    format!("Situation:\n{s}")
}

fn join(situation: Option<&str>, blocks: &[String]) -> String {
    let head = situation.map(situation_block);
    head.into_iter()
        .chain(blocks.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `label` is "Excerpt" or "Excerpt 3".
fn block(label: &str, e: &Excerpt<'_>, text: &str) -> String {
    match e.title {
        Some(t) => format!("{label}: {t} (captured {})\n{text}", e.day),
        None => format!("{label} (captured {})\n{text}", e.day),
    }
}

/// The excerpt's text cut so the whole state fits; the cut alone is not exact across the seams.
fn cut_to_fit(
    situation: Option<&str>,
    label: &str,
    e: &Excerpt<'_>,
    counter: &TokenCounter,
    tokens: usize,
) -> String {
    let frame = counter.count(&join(situation, &[block(label, e, "")]));
    let mut limit = tokens.saturating_sub(frame);
    loop {
        let text = counter.cut(e.text, limit, true).unwrap_or_default();
        if limit == 0 || counter.count(&join(situation, &[block(label, e, &text)])) <= tokens {
            return text;
        }
        limit -= limit / 20 + 1;
    }
}

pub struct Excerpt<'a> {
    pub title: Option<&'a str>,
    pub day: &'a str,
    pub text: &'a str,
}

pub fn prior_state(situation: Option<&str>) -> String {
    situation.map_or_else(|| NO_INFORMATION.into(), situation_block)
}

pub fn single_state(situation: Option<&str>, e: &Excerpt<'_>) -> String {
    join(situation, &[block("Excerpt", e, e.text)])
}

/// Whole excerpts, strongest first, while they fit `tokens`; returns the state and how many went in.
/// The first is cut to fit if it alone is too large.
pub fn joint_state(
    situation: Option<&str>,
    excerpts: &[Excerpt<'_>],
    counter: &TokenCounter,
    tokens: usize,
) -> (String, usize) {
    let mut blocks: Vec<String> = Vec::new();
    for (i, e) in excerpts.iter().enumerate() {
        let label = format!("Excerpt {}", i + 1);
        blocks.push(block(&label, e, e.text));
        if counter.count(&join(situation, &blocks)) <= tokens {
            continue;
        }
        blocks.pop();
        if i == 0 {
            blocks.push(block(
                &label,
                e,
                &cut_to_fit(situation, &label, e, counter, tokens),
            ));
        }
        break;
    }
    let n = blocks.len();
    (join(situation, &blocks), n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex<'a>(title: Option<&'a str>, day: &'a str, text: &'a str) -> Excerpt<'a> {
        Excerpt { title, day, text }
    }

    #[test]
    fn the_bears_question_quotes_the_question() {
        let b = bears_question("Should I keep bge-m3?");
        assert_eq!(b.key, "bears");
        assert_eq!(b.kind, Kind::Noul);
        assert_eq!(
            b.instructions,
            "Does this excerpt contain information that helps answer the question: «Should I keep bge-m3?»?"
        );
    }

    #[test]
    fn the_prior_has_a_fixed_state_without_a_situation() {
        assert_eq!(prior_state(None), "No information.");
        assert_eq!(prior_state(Some("12 GB.")), "Situation:\n12 GB.");
    }

    #[test]
    fn the_joint_state_is_laid_out_as_the_spec_draws_it() {
        let es = [
            ex(Some("Reindex notes"), "2026-09-14", "a"),
            ex(None, "2026-10-01", "b"),
        ];
        let (s, n) = joint_state(
            Some("The GPU has 12 GB."),
            &es,
            &TokenCounter::default(),
            10_000,
        );
        assert_eq!(
            s,
            "Situation:\nThe GPU has 12 GB.\n\nExcerpt 1: Reindex notes (captured 2026-09-14)\na\n\nExcerpt 2 (captured 2026-10-01)\nb"
        );
        assert_eq!(n, 2);
    }

    #[test]
    fn the_single_state_names_the_excerpt() {
        let e = ex(Some("T"), "2026-09-14", "a");
        assert_eq!(
            single_state(Some("S"), &e),
            "Situation:\nS\n\nExcerpt: T (captured 2026-09-14)\na"
        );
    }

    #[test]
    fn excerpts_that_do_not_fit_are_left_out_whole() {
        let c = TokenCounter::default();
        let (a, b, d) = (
            "alpha ".repeat(20),
            "bravo ".repeat(20),
            "charlie ".repeat(20),
        );
        let es = [
            ex(None, "2026-01-01", &a),
            ex(None, "2026-01-02", &b),
            ex(None, "2026-01-03", &d),
        ];
        let (s, n) = joint_state(None, &es, &c, 100);
        assert_eq!(n, 2, "{}", c.count(&s));
        assert!(!s.contains("charlie"));
        assert!(c.count(&s) <= 100);
    }

    #[test]
    fn a_first_excerpt_too_large_alone_is_cut() {
        let c = TokenCounter::default();
        let big = "word ".repeat(5000);
        let (s, n) = joint_state(None, &[ex(None, "2026-01-01", &big)], &c, 500);
        assert_eq!(n, 1);
        assert!(c.count(&s) <= 500);
    }

    #[test]
    fn without_a_situation_there_is_no_situation_block() {
        let es = [ex(None, "2026-01-01", "a")];
        let (j, _) = joint_state(None, &es, &TokenCounter::default(), 1000);
        assert!(j.starts_with("Excerpt"));
        assert!(single_state(None, &es[0]).starts_with("Excerpt"));
    }
}
