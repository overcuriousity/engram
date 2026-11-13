//! Decide: the caller's questions about a state, answered as probabilities by a decision model.

use crate::error::{Error, Result};
use crate::infer::budget::TokenCounter;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

pub mod wire;

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub key: String,
    pub kind: Kind,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// (name, description); an empty description sends the name.
    Choice { options: Vec<(String, String)> },
    /// Lowest first.
    Score { levels: Vec<String> },
    Noul,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// Probabilities in option order.
    Choice { pick: String, probabilities: Vec<f64>, confidence: f64 },
    /// Probabilities in level order.
    Score { score: f64, probabilities: Vec<f64>, confidence: f64 },
    Noul { yes: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answers {
    pub answers: HashMap<String, Answer>,
    /// As the server names itself.
    pub model: String,
    pub input_tokens: u64,
}

#[async_trait]
pub trait Decider: Send + Sync {
    async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers>;
    fn context_tokens(&self) -> usize;
    /// As configured.
    fn model(&self) -> &str;
}

pub const MAX_QUESTIONS: usize = 16;
const MAX_KEY: usize = 30;
const OPTIONS: std::ops::RangeInclusive<usize> = 2..=8;
const LEVELS: std::ops::RangeInclusive<usize> = 2..=10;

impl Question {
    pub fn noul(key: &str, instructions: &str) -> Self {
        Question { key: key.into(), kind: Kind::Noul, instructions: instructions.into() }
    }
}

/// JavaScript's `trim` (ECMAScript whitespace), which differs from `str::trim` at U+FEFF and U+0085; magicshell's fixtures depend on it.
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{D}'
                | '\u{20}'
                | '\u{A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

fn refuse(sentence: String) -> Error {
    Error::Validation(format!("decide: {sentence}"))
}

fn key_ok(k: &str) -> bool {
    (1..=MAX_KEY).contains(&k.chars().count())
        && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn validate(questions: &[Question]) -> Result<()> {
    if questions.is_empty() {
        return Err(refuse("ask at least one question.".into()));
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(refuse(format!("ask at most {MAX_QUESTIONS} questions.")));
    }
    let mut keys = HashSet::new();
    for q in questions {
        let k = &q.key;
        if !key_ok(k) {
            return Err(refuse(format!("question \"{k}\" needs a key of 1 to {MAX_KEY} lower-case letters, digits or _.")));
        }
        if !keys.insert(k.as_str()) {
            return Err(refuse(format!("question \"{k}\" shares its key with another.")));
        }
        if trim(&q.instructions).is_empty() {
            return Err(refuse(format!("question \"{k}\" has no instructions.")));
        }
        match &q.kind {
            Kind::Choice { options } => {
                if !OPTIONS.contains(&options.len()) {
                    return Err(refuse(format!("question \"{k}\" needs 2 to 8 options.")));
                }
                let mut names = HashSet::new();
                for (name, _) in options {
                    let name = trim(name);
                    if name.is_empty() {
                        return Err(refuse(format!("question \"{k}\" has an option without a name.")));
                    }
                    if !names.insert(name) {
                        return Err(refuse(format!("question \"{k}\" has the option \"{name}\" twice.")));
                    }
                }
            }
            Kind::Score { levels } => {
                if !LEVELS.contains(&levels.len()) {
                    return Err(refuse(format!("question \"{k}\" needs 2 to 10 levels.")));
                }
                if levels.iter().any(|l| trim(l).is_empty()) {
                    return Err(refuse(format!("question \"{k}\" has a level without text.")));
                }
            }
            Kind::Noul => {}
        }
    }
    Ok(())
}

pub const MARGIN_TOKENS: usize = 256;

/// Tokens left for `state`: the window, less the questions, less the margin. Saturates at 0.
pub fn budget(counter: &TokenCounter, context_tokens: usize, questions: &[Question]) -> usize {
    context_tokens.saturating_sub(counter.count(&wire::questions_json(questions)) + MARGIN_TOKENS)
}

/// `validate`, then the fit. Every Decider calls it before anything else.
pub fn check(counter: &TokenCounter, context_tokens: usize, state: &str, questions: &[Question]) -> Result<()> {
    validate(questions)?;
    let (n, b) = (counter.count(state), budget(counter, context_tokens, questions));
    if n > b {
        return Err(refuse(format!("the state is {n} tokens and {b} fit")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(key: &str, names: &[&str]) -> Question {
        Question {
            key: key.into(),
            instructions: "Which?".into(),
            kind: Kind::Choice { options: names.iter().map(|n| (n.to_string(), String::new())).collect() },
        }
    }
    fn score(key: &str, levels: &[&str]) -> Question {
        Question {
            key: key.into(),
            instructions: "How much?".into(),
            kind: Kind::Score { levels: levels.iter().map(|l| l.to_string()).collect() },
        }
    }
    fn refused(qs: &[Question]) -> bool {
        matches!(validate(qs), Err(Error::Validation(_)))
    }

    #[test]
    fn one_noul_is_valid() {
        assert!(validate(&[Question::noul("verdict", "Is it?")]).is_ok());
    }
    #[test]
    fn no_question_is_refused() {
        assert!(refused(&[]));
    }
    #[test]
    fn sixteen_pass_and_seventeen_do_not() {
        let qs: Vec<Question> = (0..17).map(|i| Question::noul(&format!("q{i}"), "Is it?")).collect();
        assert!(validate(&qs[..16]).is_ok());
        assert!(refused(&qs));
    }
    #[test]
    fn keys_are_lower_case_digits_and_underscore() {
        for bad in ["", "Verdict", "a-b", "a b", &"k".repeat(31)] {
            assert!(refused(&[Question::noul(bad, "Is it?")]), "{bad:?}");
        }
        for good in ["a", "a_1", &"k".repeat(30)] {
            assert!(validate(&[Question::noul(good, "Is it?")]).is_ok(), "{good:?}");
        }
    }
    #[test]
    fn two_questions_cannot_share_a_key() {
        assert!(refused(&[Question::noul("a", "x?"), Question::noul("a", "y?")]));
    }
    #[test]
    fn instructions_of_only_whitespace_are_refused() {
        assert!(refused(&[Question::noul("a", "  \n")]));
    }
    #[test]
    fn a_choice_takes_two_to_eight_options() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
        for (n, ok) in [(1, false), (2, true), (8, true), (9, false)] {
            assert_eq!(validate(&[choice("c", &names[..n])]).is_ok(), ok, "{n}");
        }
    }
    #[test]
    fn option_names_are_unique_after_trimming() {
        assert!(refused(&[choice("c", &["keep", " keep "])]));
        assert!(refused(&[choice("c", &["keep", ""])]));
    }
    #[test]
    fn a_score_takes_two_to_ten_levels() {
        let levels = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"];
        for (n, ok) in [(1, false), (2, true), (10, true), (11, false)] {
            assert_eq!(validate(&[score("s", &levels[..n])]).is_ok(), ok, "{n}");
        }
        assert!(refused(&[score("s", &["a", " "])]));
    }
    #[test]
    fn the_refusal_names_the_question() {
        let Err(Error::Validation(m)) = validate(&[choice("pick_me", &["a"])]) else { panic!() };
        assert!(m.contains("pick_me") && m.starts_with("decide: "), "{m}");
    }

    fn noul_q() -> [Question; 1] {
        [Question::noul("verdict", "Is it?")]
    }
    #[test]
    fn the_budget_is_the_window_less_questions_and_margin() {
        let c = TokenCounter::default();
        let qs = noul_q();
        assert_eq!(budget(&c, 8192, &qs), 8192 - c.count(&wire::questions_json(&qs)) - 256);
    }
    #[test]
    fn a_window_smaller_than_its_questions_leaves_nothing() {
        assert_eq!(budget(&TokenCounter::default(), 100, &noul_q()), 0);
    }
    #[test]
    fn a_state_at_the_budget_fits_and_one_past_it_does_not() {
        let c = TokenCounter::default();
        let qs = noul_q();
        let b = budget(&c, 2048, &qs);
        let mut state = String::new();
        while c.count(&state) < b {
            state.push('x');
        }
        assert_eq!(c.count(&state), b);
        assert!(check(&c, 2048, &state, &qs).is_ok());
        while c.count(&state) <= b {
            state.push('x');
        }
        let n = c.count(&state);
        let Err(Error::Validation(m)) = check(&c, 2048, &state, &qs) else { panic!() };
        assert!(m.contains(&n.to_string()) && m.contains(&b.to_string()), "{m}");
    }
    #[test]
    fn check_refuses_invalid_questions_first() {
        assert!(matches!(check(&TokenCounter::default(), 2048, "x", &[]), Err(Error::Validation(_))));
    }
}
