//! The after-write check: the decision model reads a rewrite against its originals before it is
//! stored. A refusal line, not a pass line: a pass certifies nothing, and the literal checks run first.

use super::Asked;
use super::band::{Band, pick_and_top};
use super::catalogue::{adds_nothing, complete};
use super::state::{self, Note};
use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::decide::Answer;
use crate::store::decision_log::{NewLogged, Outcome, State};

#[derive(Debug, Clone, PartialEq)]
pub enum Checked {
    Passed,
    Refused { question: &'static str, yes: f64 },
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Both,
    AddsNothingOnly,
}

/// One request. Logs a decision of kind "write" on `subject`; a refusal sets its outcome.
/// One row per question asked: band `act` at or above `decide.refuse_below`, `leave` and `refused` below.
pub async fn check(
    core: &Core,
    subject: &str,
    originals: &[Note<'_>],
    rewrite: &Note<'_>,
    which: Which,
) -> Result<Checked> {
    let asked = match which {
        Which::Both => vec![complete(), adds_nothing()],
        Which::AddsNothingOnly => vec![adds_nothing()],
    };
    let refs: Vec<&Asked> = asked.iter().collect();
    // Not judged on cut text: the literal checks alone decide.
    let Some(shown) = state::rewrite(originals, rewrite, &core.counter, state::room(core, &refs))
    else {
        return Ok(Checked::TooLong);
    };
    let questions: Vec<_> = asked.iter().map(|a| a.question.clone()).collect();
    let permit = core.gate.background_light().await;
    let got = core.decider.decide(&shown, &questions).await;
    permit.finished();
    let answers = got?.answers;
    let read = asked
        .iter()
        .map(|a| match answers.get(&a.question.key) {
            Some(answer @ Answer::Noul { yes }) => Ok((a, answer, *yes)),
            _ => Err(Error::InferenceRejected {
                role: "decide",
                detail: format!("the answer has no noul for \"{}\"", a.id),
            }),
        })
        .collect::<Result<Vec<_>>>()?;
    let line = core.decide_cfg.refuse_below;
    let mut out = Checked::Passed;
    for (a, answer, yes) in read {
        let refused = yes < line;
        let (pick, top) = pick_and_top(&a.question, answer);
        let id = core
            .store
            .log_decision(&NewLogged {
                question_id: a.id.into(),
                question_version: a.version,
                model: core.decider.model().into(),
                subject_kind: "write",
                subject_a: subject.into(),
                subject_b: None,
                rev_a: None,
                rev_b: None,
                answer: super::weights(&a.question, answer).to_string(),
                top,
                pick,
                swapped_agree: None,
                band: if refused { Band::Leave } else { Band::Act },
                reason: None,
                state: State::Done,
            })
            .await?;
        if refused {
            core.store
                .set_decision_outcome(&id, Outcome::Refused, None)
                .await?;
            if out == Checked::Passed {
                out = Checked::Refused {
                    question: a.id,
                    yes,
                };
            }
        }
    }
    Ok(out)
}

/// A check that failed is the writer unit's outage: retried by the queue, never taken as a refusal.
pub fn outage(e: Error) -> Error {
    match e {
        Error::Inference { .. } | Error::InferenceBusy { .. } => e,
        other => Error::Inference {
            role: "decide",
            detail: other.to_string(),
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::infer::decide::{Answer, Answers, Question};
    use crate::infer::fake::RuleDecider;

    pub(crate) const KEYS: [&str; 2] = ["complete", "adds_nothing"];

    /// Whether a request is an after-write check.
    pub(crate) fn is_check(qs: &[Question]) -> bool {
        qs.iter().any(|q| KEYS.contains(&q.key.as_str()))
    }

    /// Requests other than after-write checks.
    pub(crate) fn not_checks(d: &RuleDecider) -> usize {
        d.requests().iter().filter(|(_, qs)| !is_check(qs)).count()
    }

    /// The check's answers: `complete` says `c`, `adds_nothing` says `a`.
    pub(crate) fn answering(qs: &[Question], c: f64, a: f64) -> Option<Answers> {
        if !is_check(qs) {
            return None;
        }
        let answers = qs
            .iter()
            .map(|q| {
                let yes = if q.key == "complete" { c } else { a };
                (q.key.clone(), Answer::Noul { yes })
            })
            .collect();
        Some(Answers {
            answers,
            model: "rule-decider".into(),
            input_tokens: 0,
        })
    }

    pub(crate) fn judge(c: f64, a: f64) -> RuleDecider {
        RuleDecider::new(move |_, qs| {
            answering(qs, c, a)
                .ok_or_else(|| crate::error::Error::Validation("a check only".into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::judge;
    use super::*;
    use crate::core::judging::band::{Band, era};
    use crate::core::test_support::test_core;
    use crate::store::decision_log::{Logged, Outcome, State};
    use std::sync::Arc;

    fn note<'a>(title: &'a str, text: &'a str) -> Note<'a> {
        Note {
            title: Some(title),
            day: None,
            text,
        }
    }

    async fn row(core: &Core, question: &str, subject: &str) -> Option<Logged> {
        core.store
            .last_logged(question, subject, None, &era(core.decider.model(), 1))
            .await
            .unwrap()
    }

    async fn checked(
        c: f64,
        a: f64,
        which: Which,
    ) -> (Core, Arc<crate::infer::fake::RuleDecider>, Checked) {
        let mut core = test_core().await;
        let d = Arc::new(judge(c, a));
        core.decider = d.clone();
        let originals = [
            note("Ports", "Use 8080."),
            note("Ports v2", "Use 9090 too."),
        ];
        let got = check(
            &core,
            "s1",
            &originals,
            &note("Ports", "Use 8080 and 9090."),
            which,
        )
        .await
        .unwrap();
        (core, d, got)
    }

    #[tokio::test]
    async fn a_faithful_rewrite_passes_at_fifty_five() {
        let (core, d, got) = checked(0.55, 0.87, Which::Both).await;
        assert_eq!(got, Checked::Passed);
        assert_eq!(d.calls(), 1, "one request");
        for q in ["complete", "adds_nothing"] {
            let r = row(&core, q, "s1").await.expect("a decision row");
            assert_eq!(r.subject_kind, "write");
            assert_eq!(r.state, State::Done);
            assert_eq!(r.band, Band::Act);
            assert_eq!(r.outcome, None, "a pass changes nothing else");
            assert_eq!(r.reason, None);
        }
        let r = row(&core, "complete", "s1").await.unwrap();
        let answer: serde_json::Value = serde_json::from_str(&r.answer).unwrap();
        assert_eq!(answer["yes"], serde_json::json!(0.55));
        // Never in the sweep: nothing waits or stays open.
        assert!(core.store.waiting_acts(10).await.unwrap().is_empty());
        assert!(
            core.store
                .unsettled_acts(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_rewrite_that_dropped_a_sentence_is_refused() {
        let (core, _, got) = checked(0.03, 0.9, Which::Both).await;
        assert_eq!(
            got,
            Checked::Refused {
                question: "complete",
                yes: 0.03
            }
        );
        let r = row(&core, "complete", "s1").await.unwrap();
        assert_eq!(r.band, Band::Leave);
        assert_eq!(r.state, State::Done);
        assert_eq!(r.outcome, Some(Outcome::Refused));
    }

    #[tokio::test]
    async fn a_rewrite_that_added_a_fact_is_refused() {
        let (core, _, got) = checked(0.9, 0.04, Which::Both).await;
        assert_eq!(
            got,
            Checked::Refused {
                question: "adds_nothing",
                yes: 0.04
            }
        );
        let r = row(&core, "adds_nothing", "s1").await.unwrap();
        assert_eq!(r.outcome, Some(Outcome::Refused));
        assert_eq!(r.band, Band::Leave);
    }

    #[tokio::test]
    async fn exactly_the_line_passes() {
        let (_, _, got) = checked(0.25, 0.25, Which::Both).await;
        assert_eq!(got, Checked::Passed);
        let (_, _, got) = checked(0.2499, 0.9, Which::Both).await;
        assert!(matches!(got, Checked::Refused { .. }));
    }

    #[tokio::test]
    async fn adds_nothing_only_asks_one_question() {
        let (core, d, got) = checked(0.01, 0.9, Which::AddsNothingOnly).await;
        assert_eq!(got, Checked::Passed, "complete was not asked");
        let reqs = d.requests();
        assert_eq!(reqs.len(), 1);
        let keys: Vec<&str> = reqs[0].1.iter().map(|q| q.key.as_str()).collect();
        assert_eq!(keys, ["adds_nothing"]);
        assert!(row(&core, "complete", "s1").await.is_none());
        assert!(row(&core, "adds_nothing", "s1").await.is_some());
    }

    #[tokio::test]
    async fn a_pair_that_does_not_fit_is_not_judged() {
        let mut core = test_core().await;
        let d = Arc::new(judge(0.01, 0.01));
        d.set_context_tokens(1100);
        core.decider = d.clone();
        let long = "word ".repeat(2000);
        let got = check(
            &core,
            "s1",
            &[note("O", &long)],
            &note("W", "short"),
            Which::Both,
        )
        .await
        .unwrap();
        assert_eq!(got, Checked::TooLong);
        assert_eq!(d.calls(), 0);
        assert!(row(&core, "complete", "s1").await.is_none());
        assert!(row(&core, "adds_nothing", "s1").await.is_none());
    }

    #[tokio::test]
    async fn an_answer_without_the_question_is_an_error() {
        let mut core = test_core().await;
        core.decider = Arc::new(crate::infer::fake::RuleDecider::new(|_, _| {
            Ok(crate::infer::decide::Answers {
                answers: Default::default(),
                model: "rule-decider".into(),
                input_tokens: 0,
            })
        }));
        let got = check(&core, "s1", &[note("O", "x")], &note("W", "x"), Which::Both).await;
        assert!(got.is_err());
        assert!(outage(got.unwrap_err()).retryable(), "the unit is retried");
    }
}
