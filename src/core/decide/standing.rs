use crate::config::DecideConfig;
use crate::infer::decide::{Answer, Kind, Question, trim};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    Settled,
    Leaning,
    Split,
    NotHeld,
}

impl Standing {
    pub fn as_str(self) -> &'static str {
        match self {
            Standing::Settled => "settled",
            Standing::Leaning => "leaning",
            Standing::Split => "split",
            Standing::NotHeld => "not_held",
        }
    }
}

/// The largest probability; for a noul the larger of yes and no.
pub fn top(a: &Answer) -> f64 {
    match a {
        Answer::Choice { probabilities, .. } | Answer::Score { probabilities, .. } => {
            probabilities.iter().copied().fold(0.0, f64::max)
        }
        Answer::Noul { yes } => yes.max(1.0 - yes),
    }
}

/// Which answer this is, as an index: option, level, or 0 for yes and 1 for no.
pub fn pick(q: &Question, a: &Answer) -> usize {
    let first_max = |p: &[f64]| {
        p.iter()
            .enumerate()
            .fold((0, f64::MIN), |b, (i, &v)| if v > b.1 { (i, v) } else { b })
            .0
    };
    match (a, &q.kind) {
        (Answer::Choice { pick, .. }, Kind::Choice { options }) => {
            // The decider only returns a named option; 0 is a harmless floor.
            options
                .iter()
                .position(|(n, _)| trim(n) == pick)
                .unwrap_or(0)
        }
        (Answer::Choice { probabilities, .. } | Answer::Score { probabilities, .. }, _) => {
            first_max(probabilities)
        }
        (Answer::Noul { yes }, _) => usize::from(*yes < 0.5),
    }
}

/// `reads`: each evidence passage's bearing and its own answer to this question.
pub fn standing(
    q: &Question,
    joint: &Answer,
    swapped_agree: Option<bool>,
    reads: &[(f64, &Answer)],
    cfg: &DecideConfig,
) -> Standing {
    if swapped_agree == Some(false) {
        return Standing::Split;
    }
    let want = pick(q, joint);
    let total: f64 = reads.iter().map(|(b, _)| b).sum();
    let against: f64 = reads
        .iter()
        .filter(|(_, a)| pick(q, a) != want)
        .map(|(b, _)| b)
        .sum();
    if total > 0.0 && against / total >= cfg.split_at {
        Standing::Split
    } else if top(joint) >= cfg.settled_at {
        Standing::Settled
    } else {
        Standing::Leaning
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c2() -> Question {
        Question {
            key: "c".into(),
            kind: Kind::Choice {
                options: vec![
                    ("keep".into(), String::new()),
                    ("switch".into(), String::new()),
                ],
            },
            instructions: "Keep?".into(),
        }
    }

    fn ch(name: &str, top: f64) -> Answer {
        let p = if name == "keep" {
            vec![top, 1.0 - top]
        } else {
            vec![1.0 - top, top]
        };
        Answer::Choice {
            pick: name.into(),
            probabilities: p,
            confidence: top,
        }
    }

    #[test]
    fn top_and_pick_read_each_kind() {
        let q = c2();
        let a = ch("keep", 0.81);
        assert_eq!((top(&a), pick(&q, &a)), (0.81, 0));
        let n = Question::noul("n", "Yes?");
        let a = Answer::Noul { yes: 0.3 };
        assert_eq!((top(&a), pick(&n, &a)), (0.7, 1));
        assert_eq!(pick(&n, &Answer::Noul { yes: 0.5 }), 0);
        let s = Question {
            key: "s".into(),
            kind: Kind::Score {
                levels: vec!["a".into(), "b".into(), "c".into()],
            },
            instructions: "How?".into(),
        };
        let a = Answer::Score {
            score: 1.5,
            probabilities: vec![0.1, 0.45, 0.45],
            confidence: 0.5,
        };
        assert_eq!((top(&a), pick(&s, &a)), (0.45, 1));
    }

    #[test]
    fn a_pick_matches_an_option_named_with_whitespace_around_it() {
        let q = Question {
            key: "c".into(),
            kind: Kind::Choice {
                options: vec![
                    ("switch".into(), String::new()),
                    (" keep\n".into(), String::new()),
                ],
            },
            instructions: "Keep?".into(),
        };
        assert_eq!(pick(&q, &ch("keep", 0.9)), 1);
    }

    #[test]
    fn eighty_is_settled_and_just_under_is_leaning() {
        let (q, cfg) = (c2(), DecideConfig::default());
        for (t, want) in [(0.80, Standing::Settled), (0.79, Standing::Leaning)] {
            let j = ch("keep", t);
            let r = ch("keep", 0.9);
            assert_eq!(standing(&q, &j, None, &[(0.9, &r), (0.8, &r)], &cfg), want);
        }
    }

    #[test]
    fn swapped_picks_that_disagree_split() {
        let j = ch("keep", 0.95);
        let s = standing(&c2(), &j, Some(false), &[], &DecideConfig::default());
        assert_eq!(s, Standing::Split);
    }

    #[test]
    fn a_third_of_the_bearing_against_splits() {
        let (q, cfg, j) = (c2(), DecideConfig::default(), ch("keep", 0.95));
        let (k, w) = (ch("keep", 0.9), ch("switch", 0.9));
        let reads = [(0.9, &k), (0.9, &k), (0.93, &w)];
        assert_eq!(standing(&q, &j, None, &reads, &cfg), Standing::Split);
        let reads = [(0.9, &k), (0.9, &k), (0.9, &w)];
        assert_eq!(standing(&q, &j, None, &reads, &cfg), Standing::Settled);
    }

    #[test]
    fn no_reads_is_judged_on_the_joint_answer_alone() {
        let j = ch("keep", 0.85);
        assert_eq!(
            standing(&c2(), &j, None, &[], &DecideConfig::default()),
            Standing::Settled
        );
    }
}
