//! What a verdict does to the base: it stamps the notes that agree and queues the ones against for the pair judge.

use super::standing::pick;
use super::{refuse, wire};
use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::decide::{Answer, Kind, Question, trim};
use crate::store::asks::AskVerdict;

/// What the person corrected a wrong verdict to, as the client sends it.
#[derive(Debug, Clone, PartialEq)]
pub enum Correct {
    Option(String),
    Level(usize),
    Yes,
    No,
}

pub const MAX_QUEUED_PAIRS: usize = 3;
pub const PAIR_DETAIL: &str = "decide: read against a confirmed answer";

/// Artifact ids, strongest bearing first.
#[derive(Debug, Clone, PartialEq)]
pub struct Sides {
    pub agree: Vec<String>,
    pub against: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub verdict: Option<AskVerdict>,
    /// The stored JSON, `{key: answer}`.
    pub correct: Option<String>,
}

fn answers_to(q: &Question) -> usize {
    match &q.kind {
        Kind::Choice { options } => options.len(),
        Kind::Score { levels } => levels.len(),
        Kind::Noul => 2,
    }
}

/// `pick`'s index for what the person named; a client may spell yes and no either way.
fn told(q: &Question, c: &Correct) -> Result<usize> {
    let named = match (&q.kind, c) {
        (Kind::Choice { options }, Correct::Option(_) | Correct::Yes | Correct::No) => {
            let want = match c {
                Correct::Option(n) => trim(n),
                Correct::Yes => "yes",
                _ => "no",
            };
            options.iter().position(|(n, _)| trim(n) == want)
        }
        (Kind::Score { levels }, Correct::Level(i)) => (*i < levels.len()).then_some(*i),
        (Kind::Noul, Correct::Yes) => Some(0),
        (Kind::Noul, Correct::No) => Some(1),
        (Kind::Noul, Correct::Option(n)) => match trim(n) {
            "yes" => Some(0),
            "no" => Some(1),
            _ => None,
        },
        _ => None,
    };
    named.ok_or_else(|| refuse(&format!("the correction names no answer to \"{}\".", q.key)))
}

/// The right answer's index for this question (as `pick` counts), when it can be known:
/// Right -> the joint pick; Wrong -> the other of two, else `correct`; otherwise None.
pub fn right_answer(
    q: &Question,
    joint: &Answer,
    verdict: AskVerdict,
    correct: Option<&Correct>,
) -> Result<Option<usize>> {
    let own = pick(q, joint);
    match verdict {
        AskVerdict::Right => Ok(Some(own)),
        AskVerdict::NothingHere => Ok(None),
        AskVerdict::Wrong => {
            let named = correct.map(|c| told(q, c)).transpose()?;
            if named == Some(own) {
                return Err(refuse("the correction is the answer judged wrong."));
            }
            Ok(if answers_to(q) == 2 {
                Some(1 - own)
            } else {
                named
            })
        }
    }
}

pub fn sides(q: &Question, right: usize, reads: &[(String, f64, Answer)]) -> Sides {
    let mut by: Vec<&(String, f64, Answer)> = reads.iter().collect();
    by.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (agree, against): (Vec<_>, Vec<_>) = by.into_iter().partition(|r| pick(q, &r.2) == right);
    let ids = |v: Vec<&(String, f64, Answer)>| v.into_iter().map(|r| r.0.clone()).collect();
    Sides {
        agree: ids(agree),
        against: ids(against),
    }
}

/// As the client names it: option name, level index, or yes / no.
fn answer_value(q: &Question, i: usize) -> serde_json::Value {
    match &q.kind {
        Kind::Choice { options } => trim(&options[i].0).into(),
        Kind::Score { .. } => i.into(),
        Kind::Noul => if i == 0 { "yes" } else { "no" }.into(),
    }
}

impl Core {
    /// On the first question. Never deletes, retires or rewrites a note: it stamps, queues a pair, or opens a gap.
    pub async fn judge_decision(
        &self,
        id: &str,
        verdict: AskVerdict,
        correct: Option<Correct>,
    ) -> Result<Judged> {
        let d = self.store.decision(id).await?.ok_or(Error::NotFound)?;
        let questions = wire::stored_questions(&d.questions)?;
        let q = questions
            .first()
            .ok_or_else(|| Error::Internal("a stored decision has no question".into()))?;
        // Not held: there is no pick to be right or wrong about, so no right answer.
        let joint = d.answers.as_deref().filter(|_| !d.not_held).and_then(|a| {
            let got = wire::stored_answer(q, a);
            if got.is_none() {
                tracing::warn!(
                    decision = id,
                    "a held decision's answer does not read; the verdict is stored without effects"
                );
            }
            got
        });
        let right = match &joint {
            Some(j) => right_answer(q, j, verdict, correct.as_ref())?,
            None => None,
        };
        let stored =
            right.map(|i| serde_json::json!({ q.key.as_str(): answer_value(q, i) }).to_string());
        self.store
            .judge_decision(id, verdict, stored.as_deref())
            .await?;

        // Best-effort past this point: the verdict is written.
        if let Some(right) = right {
            let reads: Vec<(String, f64, Answer)> = d
                .evidence
                .iter()
                .filter(|e| e.bearing)
                .filter_map(|e| {
                    let got = wire::stored_answer(q, &e.read);
                    if got.is_none() {
                        tracing::warn!(decision = id, artifact_id = %e.artifact_id, "a note's own reading does not read; it is left out of the verdict");
                    }
                    got.map(|a| (e.artifact_id.clone(), e.bears, a))
                })
                .collect();
            let s = sides(q, right, &reads);
            self.confirm_cited(&s.agree).await;
            if let Some(with) = s.agree.first() {
                for against in s.against.iter().take(MAX_QUEUED_PAIRS) {
                    if let Err(e) = self
                        .store
                        .record_pair_with_detail(against, with, 0.0, PAIR_DETAIL)
                        .await
                    {
                        tracing::warn!(decision = id, artifact_id = %against, error = %e, "could not queue a note read against a confirmed answer");
                    }
                }
            }
        }
        Ok(Judged {
            verdict: Some(verdict),
            correct: stored,
        })
    }

    /// Clears the verdict and the right answer; stamps and queued pairs stay.
    pub async fn unjudge_decision(&self, id: &str) -> Result<Judged> {
        self.store.unjudge_decision(id).await?;
        let d = self.store.decision(id).await?.ok_or(Error::NotFound)?;
        Ok(Judged {
            verdict: d.verdict,
            correct: d.correct,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::core::decide::wire;
    use crate::core::test_support::test_core;
    use crate::error::Error;
    use crate::infer::decide::{Answer, Kind, Question};
    use crate::store::asks::AskVerdict;
    use crate::store::decisions::{NewDecision, NewEvidence};
    use crate::store::pairs::PairState;

    fn question(kind: Kind) -> Question {
        Question {
            key: "verdict".into(),
            kind,
            instructions: "Keep?".into(),
        }
    }

    fn choice(names: &[&str]) -> Question {
        question(Kind::Choice {
            options: names
                .iter()
                .map(|n| (n.to_string(), String::new()))
                .collect(),
        })
    }

    fn levels(n: usize) -> Question {
        question(Kind::Score {
            levels: (0..n).map(|i| format!("level {i}")).collect(),
        })
    }

    /// 0.9 on `name`, the rest shared.
    fn choose(q: &Question, name: &str) -> Answer {
        let Kind::Choice { options } = &q.kind else {
            unreachable!()
        };
        let rest = 0.1 / (options.len() - 1) as f64;
        Answer::Choice {
            pick: name.into(),
            probabilities: options
                .iter()
                .map(|(n, _)| if n == name { 0.9 } else { rest })
                .collect(),
            confidence: 0.8,
        }
    }

    fn level(n: usize, at: usize) -> Answer {
        Answer::Score {
            score: at as f64,
            probabilities: (0..n).map(|i| if i == at { 0.8 } else { 0.0 }).collect(),
            confidence: 0.8,
        }
    }

    #[test]
    fn right_means_the_verdicts_pick() {
        let q = choice(&["a", "b", "c"]);
        let joint = choose(&q, "b");
        assert_eq!(
            right_answer(&q, &joint, AskVerdict::Right, None).unwrap(),
            Some(1)
        );
        assert_eq!(
            right_answer(
                &q,
                &joint,
                AskVerdict::Right,
                Some(&Correct::Option("zzz".into()))
            )
            .unwrap(),
            Some(1),
            "a correction given with right is ignored"
        );
    }

    #[test]
    fn wrong_between_two_means_the_other() {
        let noul = Question::noul("verdict", "Keep?");
        assert_eq!(
            right_answer(&noul, &Answer::Noul { yes: 0.9 }, AskVerdict::Wrong, None).unwrap(),
            Some(1)
        );
        let q = choice(&["keep", "switch"]);
        assert_eq!(
            right_answer(&q, &choose(&q, "keep"), AskVerdict::Wrong, None).unwrap(),
            Some(1)
        );
        assert_eq!(
            right_answer(&levels(2), &level(2, 1), AskVerdict::Wrong, None).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn wrong_among_three_needs_to_be_told() {
        let q = choice(&["a", "b", "c"]);
        let joint = choose(&q, "a");
        assert_eq!(
            right_answer(&q, &joint, AskVerdict::Wrong, None).unwrap(),
            None
        );
        assert_eq!(
            right_answer(
                &q,
                &joint,
                AskVerdict::Wrong,
                Some(&Correct::Option("c".into()))
            )
            .unwrap(),
            Some(2)
        );
        assert_eq!(
            right_answer(
                &levels(3),
                &level(3, 2),
                AskVerdict::Wrong,
                Some(&Correct::Level(0))
            )
            .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn a_correction_must_name_another_answer() {
        let q = choice(&["a", "b", "c"]);
        let joint = choose(&q, "a");
        for c in [Correct::Option("zzz".into()), Correct::Option("a".into())] {
            assert!(
                matches!(
                    right_answer(&q, &joint, AskVerdict::Wrong, Some(&c)),
                    Err(Error::Validation(_))
                ),
                "{c:?}"
            );
        }
        assert!(matches!(
            right_answer(
                &levels(3),
                &level(3, 0),
                AskVerdict::Wrong,
                Some(&Correct::Level(9))
            ),
            Err(Error::Validation(_))
        ));
        let noul = Question::noul("verdict", "Keep?");
        assert!(matches!(
            right_answer(
                &noul,
                &Answer::Noul { yes: 0.9 },
                AskVerdict::Wrong,
                Some(&Correct::Yes)
            ),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn nothing_here_names_no_right_answer() {
        let q = choice(&["a", "b"]);
        assert_eq!(
            right_answer(
                &q,
                &choose(&q, "a"),
                AskVerdict::NothingHere,
                Some(&Correct::Option("b".into()))
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn sides_split_the_bearing_notes_by_their_own_pick_strongest_first() {
        let q = choice(&["keep", "switch"]);
        let reads = vec![
            ("b".to_string(), 0.7, choose(&q, "keep")),
            ("c".to_string(), 0.6, choose(&q, "switch")),
            ("a".to_string(), 0.9, choose(&q, "keep")),
            ("d".to_string(), 0.8, choose(&q, "switch")),
        ];
        let s = sides(&q, 0, &reads);
        assert_eq!(s.agree, vec!["a", "b"]);
        assert_eq!(s.against, vec!["d", "c"]);
        let s = sides(&q, 1, &reads);
        assert_eq!(
            (s.agree, s.against),
            (
                vec!["d".into(), "c".into()],
                vec!["a".to_string(), "b".into()]
            )
        );
    }

    /// Notes stamped long ago, so a fresh stamp shows.
    async fn notes(core: &Core, n: usize) -> Vec<String> {
        let texts: Vec<String> = (0..n).map(|i| format!("note {i}")).collect();
        let rows: Vec<(&str, [f32; 2])> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), [1.0, i as f32]))
            .collect();
        let ids = crate::jobs::consolidate::tests::seed(core, &rows).await;
        for id in &ids {
            core.store.set_last_verified_at(id, 1).await.unwrap();
        }
        ids
    }

    /// `joint`: `None` is not held. Each note: (id, bears, its own pick).
    async fn recorded(
        core: &Core,
        q: &Question,
        joint: Option<&str>,
        reads: &[(&str, f64, &str)],
    ) -> String {
        let qs = std::slice::from_ref(q);
        let evidence = reads
            .iter()
            .enumerate()
            .map(|(i, (id, bears, pick))| NewEvidence {
                rank: i + 1,
                artifact_id: id.to_string(),
                bears: *bears,
                bearing: true,
                joint: true,
                n: Some(i + 1),
                read: wire::read_text(qs, &[(q.key.clone(), choose(q, pick))]),
            })
            .collect();
        let answers = joint.map(|j| {
            serde_json::json!({ "verdict": wire::answer_json(q, &choose(q, j), Some(true)) })
                .to_string()
        });
        core.store
            .record_decision(NewDecision {
                question: "Keep?".into(),
                questions: wire::questions_json(qs),
                situation: None,
                scope: None,
                filters: "{}".into(),
                answers,
                prior: "{}".into(),
                standing: "{}".into(),
                not_held: joint.is_none(),
                model: "rule".into(),
                read: reads.len(),
                dropped: 0,
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                evidence,
            })
            .await
            .unwrap()
    }

    async fn stamped(core: &Core, id: &str) -> bool {
        core.store
            .get_artifact(id)
            .await
            .unwrap()
            .last_verified_at
            .unwrap()
            > 1
    }

    async fn pair_count(core: &Core) -> i64 {
        sqlx::query_scalar!("SELECT count(*) FROM artifact_pairs")
            .fetch_one(&core.store.pool)
            .await
            .unwrap()
    }

    /// A (0.95, keep), B (0.9, keep), C (0.8, switch); the verdict keep.
    async fn keep_or_switch(core: &Core) -> (Vec<String>, String) {
        let ids = notes(core, 3).await;
        let q = choice(&["keep", "switch"]);
        let id = recorded(
            core,
            &q,
            Some("keep"),
            &[
                (&ids[0], 0.95, "keep"),
                (&ids[1], 0.9, "keep"),
                (&ids[2], 0.8, "switch"),
            ],
        )
        .await;
        (ids, id)
    }

    #[tokio::test]
    async fn right_confirms_the_notes_that_agree() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        let j = core
            .judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        assert_eq!(j.verdict, Some(AskVerdict::Right));
        assert_eq!(j.correct.as_deref(), Some(r#"{"verdict":"keep"}"#));
        assert!(stamped(&core, &ids[0]).await && stamped(&core, &ids[1]).await);
        assert!(!stamped(&core, &ids[2]).await);
    }

    #[tokio::test]
    async fn right_queues_the_note_against_with_the_strongest_that_agrees() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        let p = core
            .store
            .pair_between(&ids[2], &ids[0])
            .await
            .unwrap()
            .expect("C is queued against A");
        assert_eq!(p.state, PairState::Pending);
        assert_eq!(p.detail.as_deref(), Some(PAIR_DETAIL));
        assert_eq!(p.score, 0.0);
        assert_eq!(pair_count(&core).await, 1, "no pair names B");
    }

    #[tokio::test]
    async fn wrong_between_two_confirms_the_other_side() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        let j = core
            .judge_decision(&id, AskVerdict::Wrong, None)
            .await
            .unwrap();
        assert_eq!(j.correct.as_deref(), Some(r#"{"verdict":"switch"}"#));
        assert!(stamped(&core, &ids[2]).await);
        assert!(!stamped(&core, &ids[0]).await && !stamped(&core, &ids[1]).await);
        for a in &ids[..2] {
            assert!(core.store.pair_between(a, &ids[2]).await.unwrap().is_some());
        }
        assert_eq!(pair_count(&core).await, 2);
    }

    #[tokio::test]
    async fn wrong_among_three_without_a_correction_only_stores_the_verdict() {
        let core = test_core().await;
        let ids = notes(&core, 3).await;
        let q = choice(&["a", "b", "c"]);
        let id = recorded(
            &core,
            &q,
            Some("a"),
            &[
                (&ids[0], 0.9, "a"),
                (&ids[1], 0.8, "b"),
                (&ids[2], 0.7, "c"),
            ],
        )
        .await;
        let j = core
            .judge_decision(&id, AskVerdict::Wrong, None)
            .await
            .unwrap();
        assert_eq!((j.verdict, j.correct), (Some(AskVerdict::Wrong), None));
        for n in &ids {
            assert!(!stamped(&core, n).await);
        }
        assert_eq!(pair_count(&core).await, 0);
        let s = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!((s.verdict, s.correct), (Some(AskVerdict::Wrong), None));

        let j = core
            .judge_decision(&id, AskVerdict::Wrong, Some(Correct::Option("c".into())))
            .await
            .unwrap();
        assert_eq!(j.correct.as_deref(), Some(r#"{"verdict":"c"}"#));
        assert!(stamped(&core, &ids[2]).await);
    }

    #[tokio::test]
    async fn at_most_three_pairs_are_queued() {
        let core = test_core().await;
        let ids = notes(&core, 6).await;
        let q = choice(&["keep", "switch"]);
        let reads: Vec<(&str, f64, &str)> = std::iter::once((ids[0].as_str(), 0.95, "keep"))
            .chain(
                ids[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.as_str(), 0.5 + i as f64 * 0.1, "switch")),
            )
            .collect();
        let id = recorded(&core, &q, Some("keep"), &reads).await;
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        assert_eq!(pair_count(&core).await, 3);
        // The strongest against are the last three seeded.
        for n in &ids[3..] {
            assert!(core.store.pair_between(n, &ids[0]).await.unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn a_unanimous_decision_queues_nothing() {
        let core = test_core().await;
        let ids = notes(&core, 2).await;
        let q = choice(&["keep", "switch"]);
        let id = recorded(
            &core,
            &q,
            Some("keep"),
            &[(&ids[0], 0.9, "keep"), (&ids[1], 0.8, "keep")],
        )
        .await;
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        assert_eq!(pair_count(&core).await, 0);
        // Wrong: nobody agrees with switch, so there is no one to pair against.
        core.judge_decision(&id, AskVerdict::Wrong, None)
            .await
            .unwrap();
        assert_eq!(pair_count(&core).await, 0);
    }

    #[tokio::test]
    async fn a_pair_that_exists_is_left_as_it_is() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        core.store
            .record_settled_pair(&ids[2], &ids[0], 0.7, PairState::Dismissed)
            .await
            .unwrap();
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        let p = core
            .store
            .pair_between(&ids[2], &ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!((p.state, p.detail), (PairState::Dismissed, None));
    }

    #[tokio::test]
    async fn nothing_here_confirms_nothing_and_queues_nothing() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        let j = core
            .judge_decision(
                &id,
                AskVerdict::NothingHere,
                Some(Correct::Option("switch".into())),
            )
            .await
            .unwrap();
        assert_eq!(
            (j.verdict, j.correct),
            (Some(AskVerdict::NothingHere), None)
        );
        for n in &ids {
            assert!(!stamped(&core, n).await);
        }
        assert_eq!(pair_count(&core).await, 0);
    }

    #[tokio::test]
    async fn a_verdict_on_a_decision_not_held_touches_no_note() {
        let core = test_core().await;
        let ids = notes(&core, 2).await;
        let q = choice(&["keep", "switch"]);
        let id = recorded(
            &core,
            &q,
            None,
            &[(&ids[0], 0.9, "keep"), (&ids[1], 0.8, "switch")],
        )
        .await;
        for v in [AskVerdict::Right, AskVerdict::Wrong] {
            let j = core.judge_decision(&id, v, None).await.unwrap();
            assert_eq!((j.verdict, j.correct), (Some(v), None));
        }
        for n in &ids {
            assert!(!stamped(&core, n).await);
        }
        assert_eq!(pair_count(&core).await, 0);
    }

    #[tokio::test]
    async fn no_verdict_retires_or_rewrites_a_note() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        let before: Vec<String> = {
            let mut t = Vec::new();
            for n in &ids {
                t.push(core.store.get_artifact(n).await.unwrap().text);
            }
            t
        };
        for v in [
            AskVerdict::Right,
            AskVerdict::Wrong,
            AskVerdict::NothingHere,
        ] {
            core.judge_decision(&id, v, None).await.unwrap();
            for (n, text) in ids.iter().zip(&before) {
                let a = core.store.get_artifact(n).await.unwrap();
                assert!(a.in_results(), "{v:?}");
                assert_eq!(&a.text, text);
            }
        }
    }

    #[tokio::test]
    async fn right_then_wrong_stamps_both_sides_and_keeps_the_last_verdict() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        core.judge_decision(&id, AskVerdict::Wrong, None)
            .await
            .unwrap();
        for n in &ids {
            assert!(stamped(&core, n).await);
        }
        // (C, A) from right; (A, C) is the same pair, and (B, C) is new from wrong.
        assert_eq!(pair_count(&core).await, 2);
        for a in &ids[..2] {
            assert!(core.store.pair_between(a, &ids[2]).await.unwrap().is_some());
        }
        let s = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!(s.verdict, Some(AskVerdict::Wrong));
        assert_eq!(s.correct.as_deref(), Some(r#"{"verdict":"switch"}"#));
    }

    #[tokio::test]
    async fn undo_clears_the_verdict_and_keeps_the_stamps() {
        let core = test_core().await;
        let (ids, id) = keep_or_switch(&core).await;
        core.judge_decision(&id, AskVerdict::Right, None)
            .await
            .unwrap();
        let j = core.unjudge_decision(&id).await.unwrap();
        assert_eq!((j.verdict, j.correct), (None, None));
        let s = core.store.decision(&id).await.unwrap().unwrap();
        assert_eq!((s.verdict, s.correct), (None, None));
        assert!(stamped(&core, &ids[0]).await);
        assert_eq!(pair_count(&core).await, 1);
    }

    #[tokio::test]
    async fn judging_an_unknown_decision_is_not_found() {
        let core = test_core().await;
        assert!(matches!(
            core.judge_decision("nope", AskVerdict::Right, None).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            core.unjudge_decision("nope").await,
            Err(Error::NotFound)
        ));
    }
}
