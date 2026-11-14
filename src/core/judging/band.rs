//! The act, ask or leave rule: a probability, and whether the swapped read agreed, in; a band out.

use crate::infer::decide::{Answer, Kind, Question};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub act_at: f64,
    pub ask_at: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Act,
    Ask,
    Leave,
}

impl Band {
    pub fn as_str(self) -> &'static str {
        match self {
            Band::Act => "act",
            Band::Ask => "ask",
            Band::Leave => "leave",
        }
    }
}

/// The judge and its version: a judgement made under another era is not comparable.
pub fn era(model: &str, version: u32) -> String {
    format!("{model}/v{version}")
}

fn argmax(p: &[f64]) -> Option<(usize, f64)> {
    p.iter()
        .copied()
        .enumerate()
        .fold(None, |best, (i, v)| match best {
            Some((_, b)) if b >= v => best,
            _ => Some((i, v)),
        })
}

/// The answer's pick as a string (option name, level index, "yes"/"no") and its top probability.
pub fn pick_and_top(q: &Question, a: &Answer) -> (String, f64) {
    match (a, &q.kind) {
        (Answer::Noul { yes }, _) => {
            if *yes >= 0.5 {
                ("yes".into(), *yes)
            } else {
                ("no".into(), 1.0 - yes)
            }
        }
        (
            Answer::Choice {
                pick,
                probabilities,
                ..
            },
            Kind::Choice { options },
        ) => match argmax(probabilities) {
            Some((i, top)) if i < options.len() => (options[i].0.clone(), top),
            Some((_, top)) => (pick.clone(), top),
            None => (pick.clone(), 0.0),
        },
        (
            Answer::Choice {
                pick,
                probabilities,
                ..
            },
            _,
        ) => (pick.clone(), argmax(probabilities).map_or(0.0, |(_, t)| t)),
        (Answer::Score { probabilities, .. }, _) => {
            let (i, top) = argmax(probabilities).unwrap_or((0, 0.0));
            (i.to_string(), top)
        }
    }
}

/// A choice acts only when its swapped read agreed; `None` means no swapped read applies.
pub fn band(top: f64, swapped_agree: Option<bool>, t: Thresholds) -> Band {
    if swapped_agree == Some(false) && top >= t.act_at {
        return Band::Ask;
    }
    if top >= t.act_at {
        Band::Act
    } else if top >= t.ask_at {
        Band::Ask
    } else {
        Band::Leave
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Thresholds = Thresholds {
        act_at: 0.90,
        ask_at: 0.60,
    };

    #[test]
    fn ninety_acts_sixty_asks_and_below_is_left() {
        assert_eq!(band(0.90, None, T), Band::Act);
        assert_eq!(band(0.899, None, T), Band::Ask);
        assert_eq!(band(0.60, None, T), Band::Ask);
        assert_eq!(band(0.599, None, T), Band::Leave);
    }

    #[test]
    fn a_choice_acts_only_when_its_swapped_read_agreed() {
        assert_eq!(band(0.99, Some(false), T), Band::Ask);
        assert_eq!(band(0.99, Some(true), T), Band::Act);
        assert_eq!(band(0.50, Some(false), T), Band::Leave);
        assert_eq!(band(0.75, Some(false), T), Band::Ask);
    }

    #[test]
    fn a_noul_picks_yes_or_no_and_its_top_is_the_larger() {
        let q = Question::noul("k", "i");
        let p = |yes| pick_and_top(&q, &Answer::Noul { yes });
        assert_eq!(p(0.97), ("yes".into(), 0.97));
        let (s, t) = p(0.02);
        assert_eq!(s, "no");
        assert!((t - 0.98).abs() < 1e-9);
        assert_eq!(p(0.5), ("yes".into(), 0.5));
    }

    #[test]
    fn a_choice_and_a_score_pick_their_largest() {
        let q = Question {
            key: "k".into(),
            kind: Kind::Choice {
                options: vec![("a".into(), String::new()), ("b".into(), String::new())],
            },
            instructions: "i".into(),
        };
        let a = Answer::Choice {
            pick: "b".into(),
            probabilities: vec![0.2, 0.8],
            confidence: 0.8,
        };
        assert_eq!(pick_and_top(&q, &a), ("b".into(), 0.8));
        let q = Question {
            key: "k".into(),
            kind: Kind::Score {
                levels: vec!["lo".into(), "mid".into(), "hi".into()],
            },
            instructions: "i".into(),
        };
        let a = Answer::Score {
            score: 1.0,
            probabilities: vec![0.1, 0.7, 0.2],
            confidence: 0.7,
        };
        assert_eq!(pick_and_top(&q, &a), ("1".into(), 0.7));
    }

    #[test]
    fn an_era_names_the_model_and_the_version() {
        assert_eq!(era("clef-flash", 2), "clef-flash/v2");
    }
}
