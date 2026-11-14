//! The judgement layer: every background judgement goes through it.

pub mod band;

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::decide::{Answer, Kind, Question, decide_swapped};
use crate::store::actions::Job;
use crate::store::decision_log::{Logged, NewLogged, Outcome, State};
use crate::store::pairs::DecidedBy;
use async_trait::async_trait;
use band::{Band, Thresholds, band, era, pick_and_top};
use std::collections::HashMap;
use std::sync::Arc;

/// A built-in question: its id and version name the era with the model.
#[derive(Debug, Clone)]
pub struct Asked {
    pub id: &'static str,
    pub version: u32,
    pub question: Question,
    pub swapped: bool,
}

/// What a decision is about, and the revisions it saw.
#[derive(Debug, Clone)]
pub struct Subjects {
    pub kind: &'static str,
    pub a: String,
    pub b: Option<String>,
    pub rev_a: Option<i64>,
    pub rev_b: Option<i64>,
}

/// What a question's answers do; one per question id.
#[async_trait]
pub trait Applier: Send + Sync {
    fn question_id(&self) -> &'static str;
    /// Two or three words for the Unsure row: "Same thing?", "Worth keeping?".
    fn label(&self) -> &'static str;
    /// The job whose daily cap an act of this question spends, if any.
    fn job(&self) -> Option<Job>;
    /// The answers a person may give, as (pick, button label), for this decision.
    async fn buttons(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>>;
    /// The subjects as (id to open, title) for the row.
    async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>>;
    /// Whether the subjects are still what was asked about.
    async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool>;
    /// Carry out `pick`. Returns the journal rows written; empty when the answer journals nothing.
    async fn apply(
        &self,
        core: &Core,
        d: &Logged,
        pick: &str,
        by: DecidedBy,
    ) -> Result<Vec<String>>;
    /// Put the subject back to be asked again.
    async fn reopen(&self, core: &Core, d: &Logged) -> Result<()>;
}

#[derive(Default)]
pub struct Appliers(HashMap<&'static str, Arc<dyn Applier>>);

impl Appliers {
    pub fn register(&mut self, a: Arc<dyn Applier>) {
        self.0.insert(a.question_id(), a);
    }

    pub fn get(&self, question_id: &str) -> Option<Arc<dyn Applier>> {
        self.0.get(question_id).cloned()
    }
}

impl Core {
    /// How many decision calls an arming sweep may still arm in this tick.
    pub async fn judging_budget(&self) -> Result<usize> {
        let since = crate::store::now() - i64::from(self.decide_cfg.tick_mins) * 60;
        let used = self.store.decided_since(since).await?.max(0) as usize;
        Ok(self.decide_cfg.max_per_tick.saturating_sub(used))
    }

    /// The thresholds in force for a question: the tuner's for this era, else config.
    pub async fn thresholds(&self, a: &Asked) -> Result<Thresholds> {
        let era = era(self.decider.model(), a.version);
        Ok(match self.store.thresholds_in_force(a.id, &era).await? {
            Some(t) => t,
            None => self.decide_cfg.bands.of(a.id),
        })
    }
}

async fn logged(core: &Core, id: &str) -> Result<Logged> {
    core.store.logged(id).await?.ok_or(Error::NotFound)
}

/// Probability by pick, named as `pick_and_top` names picks; no text of the model's is kept.
fn weights(q: &Question, a: &Answer) -> String {
    let by_index = |p: &[f64]| -> Vec<(String, f64)> {
        p.iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), *v))
            .collect()
    };
    let pairs = match (a, &q.kind) {
        (Answer::Noul { yes }, _) => vec![("yes".into(), *yes), ("no".into(), 1.0 - yes)],
        (Answer::Choice { probabilities, .. }, Kind::Choice { options })
            if probabilities.len() == options.len() =>
        {
            options
                .iter()
                .map(|(n, _)| n.trim().to_string())
                .zip(probabilities.iter().copied())
                .collect()
        }
        (Answer::Choice { probabilities, .. }, _) | (Answer::Score { probabilities, .. }, _) => {
            by_index(probabilities)
        }
    };
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .into_iter()
        .map(|(k, v)| (k, serde_json::json!(v)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

/// Put the question, band it, log it. `state` is the whole state; `None` means the subjects do not fit:
/// logged as leave with reason "too_long" and no call.
pub async fn ask(core: &Core, a: &Asked, s: Subjects, state: Option<&str>) -> Result<Logged> {
    let mut row = NewLogged {
        question_id: a.id.into(),
        question_version: a.version,
        model: core.decider.model().into(),
        subject_kind: s.kind,
        subject_a: s.a,
        subject_b: s.b,
        rev_a: s.rev_a,
        rev_b: s.rev_b,
        answer: "{}".into(),
        top: 0.0,
        pick: String::new(),
        swapped_agree: None,
        band: Band::Leave,
        reason: Some("too_long".into()),
        state: State::Done,
    };
    if let Some(state) = state {
        let questions = [a.question.clone()];
        let key = &a.question.key;
        // The light turn: a decision is short and does not serve or start the cooldown.
        let permit = core.gate.background_light().await;
        let got = if a.swapped {
            decide_swapped(core.decider.as_ref(), state, &questions)
                .await
                .map(|s| (s.answers, s.agree.get(key).copied()))
        } else {
            core.decider
                .decide(state, &questions)
                .await
                .map(|a| (a, None))
        };
        permit.finished();
        let (answers, agree) = got?;
        let answer = answers
            .answers
            .get(key)
            .ok_or_else(|| Error::InferenceRejected {
                role: "decide",
                detail: format!("the answer has none for \"{key}\""),
            })?;
        let (pick, top) = pick_and_top(&a.question, answer);
        let b = band(top, agree, core.thresholds(a).await?);
        row.answer = weights(&a.question, answer);
        row.top = top;
        row.pick = pick;
        row.swapped_agree = agree;
        row.band = b;
        row.reason = None;
        row.state = if b == Band::Leave {
            State::Done
        } else {
            State::Waiting
        };
    }
    let id = core.store.log_decision(&row).await?;
    logged(core, &id).await
}

/// An act decision a stage may not carry out alone becomes an ask, with the reason. Called before `settle`.
pub async fn demote_to_ask(core: &Core, d: Logged, reason: &str) -> Result<Logged> {
    if d.band != Band::Act {
        return Ok(d);
    }
    core.store.demote_decision(&d.id, reason).await?;
    logged(core, &d.id).await
}

/// After `ask`: act now if the gates allow (else leave it waiting), seat an ask row if one is free. Returns the decision as it stands.
pub async fn settle(core: &Core, d: Logged) -> Result<Logged> {
    Ok(settle_counted(core, d).await?.0)
}

/// `settle`, and whether this call applied or seated it: a worker that lost the move did neither.
pub(crate) async fn settle_counted(core: &Core, d: Logged) -> Result<(Logged, bool)> {
    if d.state != State::Waiting {
        return Ok((d, false));
    }
    match d.band {
        Band::Act => {
            let Some(applier) = core.appliers.get(&d.question_id) else {
                return Ok((d, false));
            };
            if let Some(job) = applier.job()
                && !core.may_act(job).await?
            {
                return Ok((d, false));
            }
            // Claimed before the act, so a second worker reaching it does nothing.
            if !core
                .store
                .set_decision_state(&d.id, State::Waiting, State::Done)
                .await?
            {
                return Ok((logged(core, &d.id).await?, false));
            }
            match applier.apply(core, &d, &d.pick, DecidedBy::Model).await {
                Ok(rows) => core.store.link_decision_actions(&d.id, &rows).await?,
                Err(e) => {
                    release(core, &d.id, State::Waiting).await;
                    return Err(e);
                }
            }
            Ok((logged(core, &d.id).await?, true))
        }
        Band::Ask => {
            let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
            if core.store.seat_ask(&d.id, max).await? {
                Ok((logged(core, &d.id).await?, true))
            } else {
                Ok((d, false))
            }
        }
        Band::Leave => Ok((d, false)),
    }
}

/// Undo a claim after the work failed, so the decision is as it was and is tried again.
async fn release(core: &Core, id: &str, to: State) {
    if let Err(e) = core.store.set_decision_state(id, State::Done, to).await {
        tracing::warn!(decision = id, error = %e, "could not put a failed decision back");
    }
}

/// A person's answer to an open row. `pick` of None is Leave.
pub async fn answer(core: &Core, id: &str, pick: Option<&str>) -> Result<Logged> {
    let d = logged(core, id).await?;
    let answered = || Error::Validation("this was already answered".into());
    if d.state != State::Open {
        return Err(answered());
    }
    let Some(pick) = pick else {
        if !core
            .store
            .set_decision_state(id, State::Open, State::Done)
            .await?
        {
            return Err(answered());
        }
        return logged(core, id).await;
    };
    let applier = core
        .appliers
        .get(&d.question_id)
        .ok_or_else(|| Error::Validation("nothing here can carry out this answer".into()))?;
    if !applier
        .buttons(core, &d)
        .await?
        .iter()
        .any(|(p, _)| p == pick)
    {
        return Err(Error::Validation("that is not one of the answers".into()));
    }
    if !core
        .store
        .set_decision_state(id, State::Open, State::Done)
        .await?
    {
        return Err(answered());
    }
    // A person's pick is not subject to the daily cap.
    match applier.apply(core, &d, pick, DecidedBy::Operator).await {
        Ok(rows) => core.store.link_decision_actions(id, &rows).await?,
        Err(e) => {
            release(core, id, State::Open).await;
            return Err(e);
        }
    }
    let outcome = if pick == d.pick {
        Outcome::Confirmed
    } else {
        Outcome::Corrected
    };
    core.store
        .set_decision_outcome(id, outcome, Some(pick))
        .await?;
    logged(core, id).await
}

/// For an answer that wrote no journal row (what `answered_without_action` lists): put its subject back.
pub async fn ask_again(core: &Core, id: &str) -> Result<()> {
    let d = logged(core, id).await?;
    let listed = matches!(d.outcome, Some(Outcome::Confirmed | Outcome::Corrected))
        && d.reason.as_deref() != Some("asked_again")
        && core.store.decision_action_ids(id).await?.is_empty();
    if !listed {
        return Err(Error::Validation("this cannot be asked again".into()));
    }
    let applier = core
        .appliers
        .get(&d.question_id)
        .ok_or_else(|| Error::Validation("nothing here can ask this again".into()))?;
    applier.reopen(core, &d).await?;
    core.store.set_decision_reason(id, "asked_again").await
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::core::Core;
    use crate::error::{Error, Result};
    use crate::infer::decide::{Answer, Answers, Kind, Question};
    use crate::infer::fake::RuleDecider;
    use crate::store::actions::{Job, Kind as ActionKind, NewAction};
    use crate::store::decision_log::Logged;
    use crate::store::pairs::DecidedBy;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    pub(crate) const CHOICE: &str = "test_same";
    pub(crate) const NOUL: &str = "test_keep";

    /// Records what it was asked to do; journals one discard per apply unless told otherwise.
    pub(crate) struct TestApplier {
        id: &'static str,
        job: Option<Job>,
        journals: AtomicBool,
        fails: AtomicBool,
        changed: AtomicBool,
        applied: Mutex<Vec<(String, DecidedBy)>>,
        reopened: Mutex<Vec<String>>,
    }

    impl TestApplier {
        pub(crate) fn new(id: &'static str) -> Self {
            Self {
                id,
                job: Some(Job::Judgement),
                journals: AtomicBool::new(true),
                fails: AtomicBool::new(false),
                changed: AtomicBool::new(false),
                applied: Mutex::new(vec![]),
                reopened: Mutex::new(vec![]),
            }
        }
        pub(crate) fn spending(mut self, job: Option<Job>) -> Self {
            self.job = job;
            self
        }
        pub(crate) fn set_journals(&self, on: bool) {
            self.journals.store(on, Ordering::SeqCst);
        }
        pub(crate) fn set_failing(&self, on: bool) {
            self.fails.store(on, Ordering::SeqCst);
        }
        pub(crate) fn set_changed(&self, on: bool) {
            self.changed.store(on, Ordering::SeqCst);
        }
        /// (pick, by) per apply call.
        pub(crate) fn applied(&self) -> Vec<(String, DecidedBy)> {
            self.applied.lock().unwrap().clone()
        }
        /// Decision ids reopened.
        pub(crate) fn reopened(&self) -> Vec<String> {
            self.reopened.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Applier for TestApplier {
        fn question_id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            if self.id == CHOICE {
                "Same thing?"
            } else {
                "Worth keeping?"
            }
        }
        fn job(&self) -> Option<Job> {
            self.job
        }
        async fn buttons(&self, _: &Core, _: &Logged) -> Result<Vec<(String, String)>> {
            let b = if self.id == CHOICE {
                [("same", "Same"), ("different", "Different")]
            } else {
                [("yes", "Keep"), ("no", "Drop")]
            };
            Ok(b.iter()
                .map(|(p, l)| (p.to_string(), l.to_string()))
                .collect())
        }
        async fn subjects(&self, _: &Core, d: &Logged) -> Result<Vec<(String, String)>> {
            let mut out = vec![(d.subject_a.clone(), format!("Title {}", d.subject_a))];
            if let Some(b) = &d.subject_b {
                out.push((b.clone(), format!("Title {b}")));
            }
            Ok(out)
        }
        async fn unchanged(&self, _: &Core, _: &Logged) -> Result<bool> {
            Ok(!self.changed.load(Ordering::SeqCst))
        }
        async fn apply(
            &self,
            core: &Core,
            d: &Logged,
            pick: &str,
            by: DecidedBy,
        ) -> Result<Vec<String>> {
            self.applied.lock().unwrap().push((pick.to_string(), by));
            // A real applier awaits the store; let a second worker in here.
            tokio::task::yield_now().await;
            if self.fails.load(Ordering::SeqCst) {
                return Err(Error::Store("the test applier failed".into()));
            }
            if !self.journals.load(Ordering::SeqCst) {
                return Ok(vec![]);
            }
            let id = core
                .store
                .record_action(&NewAction {
                    job: self.job.unwrap_or(Job::Judgement),
                    kind: ActionKind::Discard,
                    subject_id: d.subject_a.clone(),
                    survivor_id: None,
                    detail: None,
                    evidence: serde_json::json!({}),
                    pair_score: None,
                })
                .await?;
            Ok(vec![id])
        }
        async fn reopen(&self, _: &Core, d: &Logged) -> Result<()> {
            self.reopened.lock().unwrap().push(d.id.clone());
            Ok(())
        }
    }

    /// The state is the answer: `"0.95"`, or `"0.99 disagree"` for a reversed read that picks otherwise.
    pub(crate) fn by_state(state: &str, qs: &[Question]) -> Result<Answers> {
        let mut words = state.split_whitespace();
        let p: f64 = words.next().and_then(|w| w.parse().ok()).unwrap_or(0.5);
        let disagree = words.next() == Some("disagree");
        let mut answers = std::collections::HashMap::new();
        for q in qs {
            let a = match &q.kind {
                Kind::Noul => Answer::Noul { yes: p },
                Kind::Choice { options } => {
                    let rest = (1.0 - p) / (options.len() - 1) as f64;
                    let reversed = q.key.ends_with('r');
                    Answer::Choice {
                        pick: if reversed && disagree {
                            "different"
                        } else {
                            "same"
                        }
                        .into(),
                        probabilities: options
                            .iter()
                            .map(|(n, _)| if n == "same" { p } else { rest })
                            .collect(),
                        confidence: p,
                    }
                }
                Kind::Score { .. } => return Err(Error::Validation("no scores here".into())),
            };
            answers.insert(q.key.clone(), a);
        }
        Ok(Answers {
            answers,
            model: "rule-decider".into(),
            input_tokens: 0,
        })
    }

    /// A test core whose decider answers by `rule` and whose only applier is `applier`.
    pub(crate) async fn core_with(
        applier: TestApplier,
        rule: impl Fn(&str, &[Question]) -> Result<Answers> + Send + Sync + 'static,
    ) -> (Core, Arc<TestApplier>, Arc<RuleDecider>) {
        let mut core = crate::core::test_support::test_core().await;
        let applier = Arc::new(applier);
        let decider = Arc::new(RuleDecider::new(rule));
        let mut appliers = Appliers::default();
        appliers.register(applier.clone());
        core.appliers = Arc::new(appliers);
        core.decider = decider.clone();
        (core, applier, decider)
    }

    /// A swapped choice: "same" or "different".
    pub(crate) fn q_choice() -> Asked {
        Asked {
            id: CHOICE,
            version: 1,
            question: Question {
                key: "same".into(),
                kind: Kind::Choice {
                    options: vec![
                        ("same".into(), String::new()),
                        ("different".into(), String::new()),
                    ],
                },
                instructions: "Are these the same thing?".into(),
            },
            swapped: true,
        }
    }

    pub(crate) fn q_noul() -> Asked {
        Asked {
            id: NOUL,
            version: 1,
            question: Question::noul("keep", "Is this worth keeping?"),
            swapped: false,
        }
    }

    pub(crate) fn subjects(a: &str) -> Subjects {
        Subjects {
            kind: "pair",
            a: a.into(),
            b: Some(format!("{a}-other")),
            rev_a: Some(1),
            rev_b: Some(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::band::{Band, Thresholds, era};
    use super::test_support::*;
    use super::*;
    use crate::error::Error;
    use crate::store::decision_log::{Outcome, State};
    use crate::store::pairs::DecidedBy;

    async fn backdate(core: &crate::core::Core, id: &str, secs: i64) {
        // runtime: test-only backdating.
        sqlx::query("UPDATE decisions SET decided_at = decided_at - ? WHERE id = ?")
            .bind(secs)
            .bind(id)
            .execute(&core.store.pool)
            .await
            .unwrap();
    }

    async fn state_of(core: &crate::core::Core, id: &str) -> State {
        core.store.logged(id).await.unwrap().unwrap().state
    }

    #[tokio::test]
    async fn a_sure_swapped_choice_acts_and_links_its_journal_rows() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
        assert_eq!(d.swapped_agree, Some(true));
        assert_eq!(d.pick, "same");
        let d = settle(&core, d).await.unwrap();
        assert_eq!(d.state, State::Done);
        assert_eq!(ap.applied(), vec![("same".to_string(), DecidedBy::Model)]);
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_ninety_nine_whose_reversed_read_disagrees_asks_and_does_nothing() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.99 disagree"))
            .await
            .unwrap();
        assert_eq!(d.band, Band::Ask);
        assert_eq!(d.swapped_agree, Some(false));
        let d = settle(&core, d).await.unwrap();
        assert_eq!(d.state, State::Open);
        assert!(ap.applied().is_empty());
    }

    #[tokio::test]
    async fn a_torn_answer_opens_a_row_and_a_weak_one_is_left() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let torn = ask(&core, &q_noul(), subjects("a"), Some("0.7"))
            .await
            .unwrap();
        assert_eq!(torn.band, Band::Ask);
        assert_eq!(torn.swapped_agree, None);
        assert_eq!(settle(&core, torn).await.unwrap().state, State::Open);
        // A weak yes is a strong-enough no only above the line: 0.45 is "no" at 0.55.
        let weak = ask(&core, &q_noul(), subjects("b"), Some("0.45"))
            .await
            .unwrap();
        assert_eq!((weak.band, weak.state), (Band::Leave, State::Done));
        assert_eq!(settle(&core, weak).await.unwrap().state, State::Done);
        assert!(ap.applied().is_empty());
    }

    #[tokio::test]
    async fn subjects_that_do_not_fit_are_left_without_a_call() {
        let (core, _, dec) = core_with(TestApplier::new(NOUL), by_state).await;
        let d = ask(&core, &q_noul(), subjects("a"), None).await.unwrap();
        assert_eq!((d.band, d.state), (Band::Leave, State::Done));
        assert_eq!(d.reason.as_deref(), Some("too_long"));
        assert_eq!(dec.calls(), 0);
    }

    #[tokio::test]
    async fn a_demoted_act_asks_with_its_reason_and_never_acts() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.99"))
            .await
            .unwrap();
        assert_eq!(d.band, Band::Act);
        let d = demote_to_ask(&core, d, "restricted").await.unwrap();
        assert_eq!(d.band, Band::Ask);
        assert_eq!(d.reason.as_deref(), Some("restricted"));
        let d = settle(&core, d).await.unwrap();
        assert_eq!(d.state, State::Open);
        assert!(ap.applied().is_empty());
        let stored = core.store.logged(&d.id).await.unwrap().unwrap();
        assert_eq!(
            (stored.band, stored.reason.as_deref()),
            (Band::Ask, Some("restricted"))
        );
    }

    #[tokio::test]
    async fn demoting_an_ask_or_a_leave_changes_nothing() {
        let (core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        for (state, band) in [("0.7", Band::Ask), ("0.5", Band::Leave)] {
            let d = ask(&core, &q_noul(), subjects(state), Some(state))
                .await
                .unwrap();
            let after = demote_to_ask(&core, d.clone(), "restricted").await.unwrap();
            assert_eq!(after.band, band);
            assert_eq!(after.reason, d.reason);
            let stored = core.store.logged(&d.id).await.unwrap().unwrap();
            assert_eq!((stored.band, stored.reason), (band, d.reason));
        }
    }

    #[tokio::test]
    async fn the_cap_makes_an_act_wait_and_a_later_sweep_applies_it_without_asking_again() {
        let (mut core, ap, dec) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        let d = settle(&core, d).await.unwrap();
        assert_eq!(d.state, State::Waiting);
        assert!(ap.applied().is_empty());
        assert_eq!(core.store.open_ask_count().await.unwrap(), 0);

        core.evolve.max_actions_per_day = 20;
        let calls = dec.calls();
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(dec.calls(), calls);
        assert_eq!(state_of(&core, &d.id).await, State::Done);
        assert_eq!(ap.applied(), vec![("same".to_string(), DecidedBy::Model)]);
    }

    #[tokio::test]
    async fn an_act_that_spends_no_job_is_not_held_by_the_cap() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE).spending(None), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        assert_eq!(settle(&core, d).await.unwrap().state, State::Done);
        assert_eq!(ap.applied().len(), 1);
    }

    #[tokio::test]
    async fn a_waiting_act_never_becomes_an_ask_row() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        settle(&core, d.clone()).await.unwrap();
        for _ in 0..3 {
            let r = crate::jobs::decisions::run(&core).await.unwrap();
            assert_eq!((r.applied, r.seated), (0, 0));
        }
        assert!(core.store.open_asks(10).await.unwrap().is_empty());
        assert_eq!(state_of(&core, &d.id).await, State::Waiting);
        assert!(ap.applied().is_empty());
    }

    #[tokio::test]
    async fn a_changed_subject_is_closed_and_asked_again_instead_of_applied() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        settle(&core, d.clone()).await.unwrap();
        ap.set_changed(true);
        core.evolve.max_actions_per_day = 20;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.reasked, r.applied), (1, 0));
        assert_eq!(ap.reopened(), vec![d.id.clone()]);
        assert!(ap.applied().is_empty());
        let stored = core.store.logged(&d.id).await.unwrap().unwrap();
        assert_eq!(stored.state, State::Done);
        assert_eq!(stored.reason.as_deref(), Some("changed"));
        // Closed: the next sweep neither applies nor reopens it again.
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.reasked, r.applied), (0, 0));
        assert_eq!(ap.reopened().len(), 1);
    }

    #[tokio::test]
    async fn an_applier_that_fails_leaves_the_decision_waiting() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        ap.set_failing(true);
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        assert!(settle(&core, d.clone()).await.is_err());
        assert_eq!(state_of(&core, &d.id).await, State::Waiting);
        assert!(
            core.store
                .decision_action_ids(&d.id)
                .await
                .unwrap()
                .is_empty()
        );
        ap.set_failing(false);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(state_of(&core, &d.id).await, State::Done);
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn two_sweeps_at_once_apply_a_decision_once() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        settle(&core, d.clone()).await.unwrap();
        core.evolve.max_actions_per_day = 20;
        let (one, two) = tokio::join!(
            crate::jobs::decisions::run(&core),
            crate::jobs::decisions::run(&core)
        );
        assert_eq!(one.unwrap().applied + two.unwrap().applied, 1);
        assert_eq!(ap.applied().len(), 1);
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn the_twenty_first_ask_waits_for_a_seat_and_takes_the_highest_top_first() {
        let (mut core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.decide_cfg.ask_open_max = 2;
        let mut ids = vec![];
        for p in ["0.7", "0.8", "0.75", "0.65"] {
            let d = ask(&core, &q_noul(), subjects(p), Some(p)).await.unwrap();
            let d = settle(&core, d).await.unwrap();
            ids.push(d.id.clone());
        }
        let states = |core: crate::core::Core, ids: Vec<String>| async move {
            let mut out = vec![];
            for id in ids {
                out.push(state_of(&core, &id).await);
            }
            out
        };
        assert_eq!(
            states(core.clone(), ids.clone()).await,
            [State::Open, State::Open, State::Waiting, State::Waiting]
        );
        answer(&core, &ids[0], None).await.unwrap();
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.seated, 1);
        assert_eq!(
            states(core.clone(), ids.clone()).await,
            [State::Done, State::Open, State::Open, State::Waiting]
        );
    }

    #[tokio::test]
    async fn a_row_nobody_answers_lapses() {
        let (core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let d = ask(&core, &q_noul(), subjects("a"), Some("0.7"))
            .await
            .unwrap();
        let d = settle(&core, d).await.unwrap();
        assert_eq!(d.state, State::Open);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.lapsed, 0);
        backdate(&core, &d.id, 31 * 86_400).await;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.lapsed, 1);
        assert_eq!(state_of(&core, &d.id).await, State::Lapsed);
    }

    async fn open_row(core: &crate::core::Core, a: &str) -> String {
        let d = ask(core, &q_noul(), subjects(a), Some("0.7"))
            .await
            .unwrap();
        let d = settle(core, d).await.unwrap();
        assert_eq!(d.state, State::Open);
        d.id
    }

    #[tokio::test]
    async fn a_person_who_picks_the_models_pick_confirms_it_past_the_cap() {
        let (mut core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let id = open_row(&core, "a").await;
        let d = answer(&core, &id, Some("yes")).await.unwrap();
        assert_eq!(ap.applied(), vec![("yes".to_string(), DecidedBy::Operator)]);
        assert_eq!(d.state, State::Done);
        assert_eq!(d.outcome, Some(Outcome::Confirmed));
        assert_eq!(d.answered_pick.as_deref(), Some("yes"));
        assert_eq!(core.store.decision_action_ids(&id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_person_who_picks_otherwise_corrects_it() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        let d = answer(&core, &id, Some("no")).await.unwrap();
        assert_eq!(ap.applied(), vec![("no".to_string(), DecidedBy::Operator)]);
        assert_eq!(d.outcome, Some(Outcome::Corrected));
        assert_eq!(d.answered_pick.as_deref(), Some("no"));
    }

    #[tokio::test]
    async fn leave_closes_the_row_and_does_nothing() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        let d = answer(&core, &id, None).await.unwrap();
        assert_eq!((d.state, d.outcome), (State::Done, None));
        assert!(ap.applied().is_empty());
        assert_eq!(core.store.open_ask_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_second_answer_is_refused_in_one_sentence() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        answer(&core, &id, Some("yes")).await.unwrap();
        for pick in [Some("no"), None] {
            let Err(Error::Validation(m)) = answer(&core, &id, pick).await else {
                panic!("a second answer went through");
            };
            assert_eq!(m, "this was already answered");
        }
        let lapsed = open_row(&core, "b").await;
        backdate(&core, &lapsed, 31 * 86_400).await;
        crate::jobs::decisions::run(&core).await.unwrap();
        assert!(matches!(
            answer(&core, &lapsed, Some("yes")).await,
            Err(Error::Validation(m)) if m == "this was already answered"
        ));
        assert_eq!(ap.applied().len(), 1);
    }

    #[tokio::test]
    async fn ask_again_reopens_only_an_answer_that_left_no_journal_row() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let open = open_row(&core, "open").await;
        assert!(ask_again(&core, &open).await.is_err());

        let journaled = open_row(&core, "journaled").await;
        answer(&core, &journaled, Some("yes")).await.unwrap();
        assert!(ask_again(&core, &journaled).await.is_err());

        ap.set_journals(false);
        let bare = open_row(&core, "bare").await;
        answer(&core, &bare, Some("no")).await.unwrap();
        let listed = core.store.answered_without_action(10).await.unwrap();
        assert_eq!(listed.len(), 1);
        ask_again(&core, &bare).await.unwrap();
        assert_eq!(ap.reopened(), vec![bare.clone()]);
        assert!(
            core.store
                .answered_without_action(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(ask_again(&core, &bare).await.is_err());
        assert_eq!(ap.reopened().len(), 1);
    }

    #[tokio::test]
    async fn the_tuners_thresholds_outrank_config_for_their_era_only() {
        let (core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let config = core.decide_cfg.bands.of(NOUL);
        let tuned = Thresholds {
            act_at: 0.65,
            ask_at: 0.55,
        };
        assert_eq!(core.thresholds(&q_noul()).await.unwrap(), config);
        core.store
            .move_thresholds(NOUL, &era(core.decider.model(), 1), tuned, "test")
            .await
            .unwrap();
        assert_eq!(core.thresholds(&q_noul()).await.unwrap(), tuned);
        let mut next = q_noul();
        next.version = 2;
        assert_eq!(core.thresholds(&next).await.unwrap(), config);
        let d = ask(&core, &q_noul(), subjects("a"), Some("0.7"))
            .await
            .unwrap();
        assert_eq!(d.band, Band::Act);
        let d = ask(&core, &next, subjects("b"), Some("0.7")).await.unwrap();
        assert_eq!(d.band, Band::Ask);
    }

    #[tokio::test]
    async fn the_budget_is_what_is_left_of_the_tick() {
        let (mut core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.decide_cfg.max_per_tick = 3;
        core.decide_cfg.tick_mins = 15;
        assert_eq!(core.judging_budget().await.unwrap(), 3);
        let old = ask(&core, &q_noul(), subjects("old"), None).await.unwrap();
        backdate(&core, &old.id, 16 * 60).await;
        for a in ["a", "b"] {
            ask(&core, &q_noul(), subjects(a), None).await.unwrap();
        }
        assert_eq!(core.judging_budget().await.unwrap(), 1);
        ask(&core, &q_noul(), subjects("c"), None).await.unwrap();
        ask(&core, &q_noul(), subjects("d"), None).await.unwrap();
        assert_eq!(core.judging_budget().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_decision_call_takes_the_light_turn_and_no_cooldown() {
        use std::time::Duration;
        let (mut core, _, dec) = core_with(TestApplier::new(NOUL), by_state).await;
        core.gate = std::sync::Arc::new(crate::infer::gate::InferenceGate::new(
            Duration::from_secs(600),
        ));
        core.gate.call_finished();
        tokio::time::timeout(
            Duration::from_secs(10),
            ask(&core, &q_noul(), subjects("a"), Some("0.7")),
        )
        .await
        .expect("a decision call waited out the cooldown")
        .unwrap();

        let lease = core.gate.interactive();
        let c = core.clone();
        let waiting =
            tokio::spawn(async move { ask(&c, &q_noul(), subjects("b"), Some("0.7")).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "a decision call ran past the lease");
        assert_eq!(dec.calls(), 1);
        drop(lease);
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the lease's end did not let it through")
            .unwrap()
            .unwrap();
        assert_eq!(dec.calls(), 2);
    }
}
