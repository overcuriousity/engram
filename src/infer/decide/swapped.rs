//! Swapped reads: each choice asked twice, options forward and reversed, so option order cannot carry the answer.

use super::{Answer, Answers, Decider, Kind, MAX_QUESTIONS, Question, validate};
use crate::error::{Error, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct SwappedAnswers {
    /// Under the caller's keys.
    pub answers: Answers,
    /// One entry per choice question.
    pub agree: HashMap<String, bool>,
}

fn rejected(what: String) -> Error {
    Error::InferenceRejected {
        role: "decide",
        detail: format!("the answer {what}"),
    }
}

/// What a swapped read sends for these questions (`q0`, `q0r`, `q1`, ...); a caller sizes its state against this.
pub fn swapped_questions(questions: &[Question]) -> Vec<Question> {
    let mut wire = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        wire.push(Question {
            key: format!("q{i}"),
            ..q.clone()
        });
        if let Kind::Choice { options } = &q.kind {
            let reversed = Kind::Choice {
                options: options.iter().rev().cloned().collect(),
            };
            wire.push(Question {
                key: format!("q{i}r"),
                kind: reversed,
                ..q.clone()
            });
        }
    }
    wire
}

fn choice_of<'a>(answers: &'a Answers, key: &str) -> Result<(&'a str, &'a [f64], f64)> {
    match answers.answers.get(key) {
        Some(Answer::Choice {
            pick,
            probabilities,
            confidence,
        }) => Ok((pick, probabilities, *confidence)),
        _ => Err(rejected(format!("has no choice for \"{key}\""))),
    }
}

/// One request. Each choice is asked twice, options forward and reversed; the answer is the mean.
pub async fn decide_swapped(
    decider: &dyn Decider,
    state: &str,
    questions: &[Question],
) -> Result<SwappedAnswers> {
    validate(questions)?;
    let wire = swapped_questions(questions);
    if wire.len() > MAX_QUESTIONS {
        return Err(Error::Validation(format!(
            "decide: a swapped read sends {} questions and {MAX_QUESTIONS} fit.",
            wire.len()
        )));
    }
    let got = decider.decide(state, &wire).await?;
    let (mut answers, mut agree) = (HashMap::new(), HashMap::new());
    for (i, q) in questions.iter().enumerate() {
        let Kind::Choice { options } = &q.kind else {
            let a = got
                .answers
                .get(&format!("q{i}"))
                .ok_or_else(|| rejected(format!("has no answer for \"q{i}\"")))?;
            answers.insert(q.key.clone(), a.clone());
            continue;
        };
        let n = options.len();
        let (fwd_pick, fwd, fwd_conf) = choice_of(&got, &format!("q{i}"))?;
        let (rev_pick, rev, rev_conf) = choice_of(&got, &format!("q{i}r"))?;
        if fwd.len() != n || rev.len() != n {
            return Err(rejected(format!(
                "for \"{}\" has the wrong number of probabilities",
                q.key
            )));
        }
        // The twin lists the options backwards.
        let mean: Vec<f64> = (0..n).map(|j| (fwd[j] + rev[n - 1 - j]) / 2.0).collect();
        let best = (1..n).fold(0, |b, j| if mean[j] > mean[b] { j } else { b });
        agree.insert(q.key.clone(), fwd_pick == rev_pick);
        answers.insert(
            q.key.clone(),
            Answer::Choice {
                pick: options[best].0.clone(),
                probabilities: mean,
                confidence: (fwd_conf + rev_conf) / 2.0,
            },
        );
    }
    Ok(SwappedAnswers {
        answers: Answers {
            answers,
            model: got.model,
            input_tokens: got.input_tokens,
        },
        agree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infer::fake::ScriptedDecider;

    fn choice(key: &str, names: &[&str]) -> Question {
        Question {
            key: key.into(),
            instructions: "Which?".into(),
            kind: Kind::Choice {
                options: names
                    .iter()
                    .map(|n| (n.to_string(), format!("about {n}")))
                    .collect(),
            },
        }
    }
    fn pick(name: &str, probabilities: &[f64], confidence: f64) -> Answer {
        Answer::Choice {
            pick: name.into(),
            probabilities: probabilities.to_vec(),
            confidence,
        }
    }
    fn scripted(wire_answers: &[(&str, Answer)]) -> ScriptedDecider {
        ScriptedDecider::new(vec![Answers {
            answers: wire_answers
                .iter()
                .map(|(k, a)| (k.to_string(), a.clone()))
                .collect(),
            model: "m".into(),
            input_tokens: 7,
        }])
    }
    fn near(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }
    fn verdict() -> Vec<Question> {
        vec![choice("verdict", &["keep", "switch"])]
    }

    #[tokio::test]
    async fn the_request_carries_each_choice_twice_reversed() {
        let qs = vec![
            choice("verdict", &["keep", "switch"]),
            Question::noul("sure", "Sure?"),
        ];
        let d = scripted(&[
            ("q0", pick("keep", &[0.5, 0.5], 0.5)),
            ("q0r", pick("keep", &[0.5, 0.5], 0.5)),
            ("q1", Answer::Noul { yes: 0.8 }),
        ]);
        decide_swapped(&d, "state", &qs).await.unwrap();
        let (state, sent) = d.requests().remove(0);
        assert_eq!(state, "state");
        assert_eq!(
            sent.iter().map(|q| q.key.as_str()).collect::<Vec<_>>(),
            ["q0", "q0r", "q1"]
        );
        let names = |q: &Question| match &q.kind {
            Kind::Choice { options } => options
                .iter()
                .map(|o| (o.0.clone(), o.1.clone()))
                .collect::<Vec<_>>(),
            _ => panic!(),
        };
        assert_eq!(
            names(&sent[0]),
            [
                ("keep".into(), "about keep".into()),
                ("switch".into(), "about switch".into())
            ]
        );
        assert_eq!(
            names(&sent[1]),
            [
                ("switch".into(), "about switch".into()),
                ("keep".into(), "about keep".into())
            ]
        );
        assert_eq!(sent[2].kind, Kind::Noul);
        assert!(sent.iter().all(|q| q.instructions
            == if q.kind == Kind::Noul {
                "Sure?"
            } else {
                "Which?"
            }));
    }
    #[tokio::test]
    async fn the_answer_is_the_mean_of_both_reads() {
        let d = scripted(&[
            ("q0", pick("keep", &[0.5, 0.5], 0.2)),
            ("q0r", pick("keep", &[0.24, 0.76], 0.6)),
        ]);
        let r = decide_swapped(&d, "", &verdict()).await.unwrap();
        let Answer::Choice {
            pick,
            probabilities,
            confidence,
        } = &r.answers.answers["verdict"]
        else {
            panic!()
        };
        assert_eq!(pick, "keep");
        assert!(near(probabilities, &[0.63, 0.37]), "{probabilities:?}");
        assert!((confidence - 0.4).abs() < 1e-9);
        assert!(r.agree["verdict"]);
        assert_eq!((r.answers.model.as_str(), r.answers.input_tokens), ("m", 7));
    }
    #[tokio::test]
    async fn picks_that_differ_are_reported() {
        let d = scripted(&[
            ("q0", pick("keep", &[0.55, 0.45], 0.55)),
            ("q0r", pick("switch", &[0.70, 0.30], 0.7)),
        ]);
        let r = decide_swapped(&d, "", &verdict()).await.unwrap();
        let Answer::Choice {
            pick,
            probabilities,
            ..
        } = &r.answers.answers["verdict"]
        else {
            panic!()
        };
        assert!(near(probabilities, &[0.425, 0.575]), "{probabilities:?}");
        assert_eq!(pick, "switch");
        assert!(!r.agree["verdict"]);
    }
    #[tokio::test]
    async fn three_options_are_mapped_back_by_name() {
        let d = scripted(&[
            ("q0", pick("a", &[0.6, 0.3, 0.1], 0.6)),
            ("q0r", pick("a", &[0.2, 0.3, 0.5], 0.5)),
        ]);
        let r = decide_swapped(&d, "", &[choice("verdict", &["a", "b", "c"])])
            .await
            .unwrap();
        let Answer::Choice { probabilities, .. } = &r.answers.answers["verdict"] else {
            panic!()
        };
        assert!(
            near(probabilities, &[0.55, 0.30, 0.15]),
            "{probabilities:?}"
        );
    }
    #[tokio::test]
    async fn a_tie_goes_to_the_earlier_option() {
        let d = scripted(&[
            ("q0", pick("keep", &[0.5, 0.5], 0.5)),
            ("q0r", pick("switch", &[0.5, 0.5], 0.5)),
        ]);
        let r = decide_swapped(&d, "", &verdict()).await.unwrap();
        let Answer::Choice { pick, .. } = &r.answers.answers["verdict"] else {
            panic!()
        };
        assert_eq!(pick, "keep");
    }
    #[tokio::test]
    async fn noul_and_score_pass_through_unpaired() {
        let qs = vec![
            Question::noul("sure", "Sure?"),
            Question {
                key: "how".into(),
                instructions: "How?".into(),
                kind: Kind::Score {
                    levels: vec!["lo".into(), "hi".into()],
                },
            },
        ];
        let score = Answer::Score {
            score: 0.4,
            probabilities: vec![0.6, 0.4],
            confidence: 0.6,
        };
        let d = scripted(&[("q0", Answer::Noul { yes: 0.8 }), ("q1", score.clone())]);
        let r = decide_swapped(&d, "", &qs).await.unwrap();
        assert_eq!(r.answers.answers["sure"], Answer::Noul { yes: 0.8 });
        assert_eq!(r.answers.answers["how"], score);
        assert!(r.agree.is_empty());
    }
    #[tokio::test]
    async fn nine_choices_do_not_fit_in_one_request() {
        let qs: Vec<Question> = (0..9)
            .map(|i| choice(&format!("c{i}"), &["a", "b"]))
            .collect();
        let d = ScriptedDecider::new(vec![]);
        assert!(matches!(
            decide_swapped(&d, "", &qs).await,
            Err(Error::Validation(_))
        ));
        assert_eq!(d.calls(), 0);
    }
    #[tokio::test]
    async fn a_bad_caller_key_is_named_as_the_caller_wrote_it() {
        let d = ScriptedDecider::new(vec![]);
        let Err(Error::Validation(m)) =
            decide_swapped(&d, "", &[choice("Verdict", &["a", "b"])]).await
        else {
            panic!()
        };
        assert!(m.contains("Verdict"), "{m}");
    }
    #[tokio::test]
    async fn a_missing_twin_answer_is_an_error() {
        let d = scripted(&[("q0", pick("keep", &[0.5, 0.5], 0.5))]);
        let e = decide_swapped(&d, "", &verdict()).await;
        assert!(matches!(e, Err(Error::InferenceRejected { .. })), "{e:?}");
    }
}
