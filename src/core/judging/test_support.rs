//! What judging tests share: an applier that records what it was asked to do, a decider that
//! answers by its state, and decisions seeded in a band.

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
    reopen_fails: AtomicBool,
    changed: AtomicBool,
    changed_subject: Mutex<Option<String>>,
    unreadable: Mutex<Option<String>>,
    disarmed: AtomicBool,
    applied: Mutex<Vec<(String, DecidedBy)>>,
    reopened: Mutex<Vec<String>>,
    buttons: Option<Vec<(String, String)>>,
    agreeing: Option<String>,
}

impl TestApplier {
    pub(crate) fn new(id: &'static str) -> Self {
        Self {
            id,
            job: Some(Job::Judgement),
            journals: AtomicBool::new(true),
            fails: AtomicBool::new(false),
            reopen_fails: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            changed_subject: Mutex::new(None),
            unreadable: Mutex::new(None),
            disarmed: AtomicBool::new(false),
            applied: Mutex::new(vec![]),
            reopened: Mutex::new(vec![]),
            buttons: None,
            agreeing: None,
        }
    }
    /// Answers other than the defaults, as (pick, label).
    pub(crate) fn with_buttons(mut self, b: &[(&str, &str)]) -> Self {
        self.buttons = Some(
            b.iter()
                .map(|(p, l)| (p.to_string(), l.to_string()))
                .collect(),
        );
        self
    }
    /// A pick that says what the model's pick said, under another name.
    pub(crate) fn agreeing_on(mut self, pick: &str) -> Self {
        self.agreeing = Some(pick.to_string());
        self
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
    /// Whether putting a subject back fails.
    pub(crate) fn set_reopen_failing(&self, on: bool) {
        self.reopen_fails.store(on, Ordering::SeqCst);
    }
    pub(crate) fn set_changed(&self, on: bool) {
        self.changed.store(on, Ordering::SeqCst);
    }
    /// Only decisions whose first subject is `a` are about something that changed.
    pub(crate) fn set_changed_subject(&self, a: &str) {
        *self.changed_subject.lock().unwrap() = Some(a.to_string());
    }
    /// The subjects of decisions whose first subject is `a` cannot be read.
    pub(crate) fn set_unreadable(&self, a: &str) {
        *self.unreadable.lock().unwrap() = Some(a.to_string());
    }
    /// Whether the base may carry out this question's acts on its own.
    pub(crate) fn set_armed(&self, on: bool) {
        self.disarmed.store(!on, Ordering::SeqCst);
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
        if let Some(b) = &self.buttons {
            return Ok(b.clone());
        }
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
        if self.unreadable.lock().unwrap().as_deref() == Some(d.subject_a.as_str()) {
            return Err(Error::Store(
                "the test applier could not read its subjects".into(),
            ));
        }
        let mut out = vec![(d.subject_a.clone(), format!("Title {}", d.subject_a))];
        if let Some(b) = &d.subject_b {
            out.push((b.clone(), format!("Title {b}")));
        }
        Ok(out)
    }
    async fn unchanged(&self, _: &Core, d: &Logged) -> Result<bool> {
        let this_one =
            self.changed_subject.lock().unwrap().as_deref() == Some(d.subject_a.as_str());
        Ok(!this_one && !self.changed.load(Ordering::SeqCst))
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
        // A real applier awaits the store; let a second worker in here.
        tokio::task::yield_now().await;
        if self.reopen_fails.load(Ordering::SeqCst) {
            return Err(Error::Store("the test applier could not reopen".into()));
        }
        self.reopened.lock().unwrap().push(d.id.clone());
        Ok(())
    }
    fn agrees(&self, d: &Logged, pick: &str) -> bool {
        pick == d.pick || self.agreeing.as_deref() == Some(pick)
    }
    fn armed(&self, _: &Core) -> bool {
        !self.disarmed.load(Ordering::SeqCst)
    }
}

/// Whether `key` is the reversed twin's in a swapped read. Mirrors `swapped_questions`
/// (`infer/decide/swapped.rs`), which sends question `i` as `q{i}` and its twin as `q{i}r`.
pub(crate) fn reversed_twin(key: &str) -> bool {
    key.strip_prefix('q')
        .and_then(|k| k.strip_suffix('r'))
        .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
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
                let reversed = reversed_twin(&q.key);
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

/// Puts the core's decider behind the tick's meter, as `Core::from_config_with` builds it.
pub(crate) fn metered(core: &mut Core) {
    core.decider = Arc::new(Metered::new(core.decider.clone(), core.calls.clone()));
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

/// A decision about `a` and `a-other` with `answer` as its weights, logged in `band` and `state`;
/// a waiting ask is settled, so it opens while there is a seat.
pub(crate) async fn seeded(
    core: &Core,
    question_id: &str,
    a: &str,
    answer: serde_json::Value,
    band: Band,
) -> Logged {
    let pick = answer
        .as_object()
        .and_then(|m| {
            m.iter()
                .max_by(|x, y| x.1.as_f64().partial_cmp(&y.1.as_f64()).unwrap())
        })
        .map(|(k, _)| k.clone())
        .unwrap_or_default();
    let top = answer[&pick].as_f64().unwrap_or(0.0);
    let id = core
        .store
        .log_decision(&NewLogged {
            question_id: question_id.into(),
            question_version: 1,
            model: core.decider.model().into(),
            subject_kind: "pair",
            subject_a: a.into(),
            subject_b: Some(format!("{a}-other")),
            rev_a: Some(1),
            rev_b: Some(1),
            answer: answer.to_string(),
            top,
            pick,
            swapped_agree: None,
            band,
            reason: None,
            state: State::Waiting,
        })
        .await
        .unwrap();
    let d = logged(core, &id).await.unwrap();
    match band {
        Band::Ask => settle(core, d).await.unwrap(),
        _ => d,
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
