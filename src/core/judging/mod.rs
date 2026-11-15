//! The judgement layer: every background judgement goes through it.

pub mod band;
pub mod catalogue;
pub mod check;
pub mod state;
#[cfg(test)]
pub(crate) mod test_support;

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::decide::{Answer, Answers, Decider, Kind, Question, decide_swapped};
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
    /// Put the subject back to be asked again. Must leave alone a subject a person has settled
    /// since: the sweep calls this for every decision whose subject is no longer `unchanged`.
    async fn reopen(&self, core: &Core, d: &Logged) -> Result<()>;
    /// Whether a person's `pick` says what the model's pick said. The buttons do not always offer
    /// the model's pick by name (a duplicate nothing can write is offered as "keep one"), and an
    /// answer that agrees must not be counted as a correction.
    fn agrees(&self, d: &Logged, pick: &str) -> bool {
        pick == d.pick
    }
    /// Whether the base may carry out this question's acts on its own as it is configured now.
    /// False leaves a sure decision waiting, whatever the daily cap says; a person's answer is
    /// never held by it.
    fn armed(&self, _core: &Core) -> bool {
        true
    }
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

    /// Every question something here can show and carry out.
    pub fn ids(&self) -> Vec<&'static str> {
        let mut ids: Vec<_> = self.0.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The questions whose acts the base may not carry out on its own as it is configured now.
    pub fn disarmed(&self, core: &Core) -> Vec<&'static str> {
        let mut ids: Vec<_> = self
            .0
            .values()
            .filter(|a| !a.armed(core))
            .map(|a| a.question_id())
            .collect();
        ids.sort_unstable();
        ids
    }
}

/// The judging calls the decision log does not hold, by when each was sent: a note's reading, a
/// search's labels, a capture's pre-read and a link's exchanged read write no decision row. In
/// memory and shared by every `Core` of one base; a restart forgets at most one tick of them.
#[derive(Default)]
pub struct Meter(std::sync::Mutex<std::collections::VecDeque<i64>>);

impl Meter {
    pub(crate) fn record(&self, at: i64) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(at);
    }

    /// Calls sent at or after `at`. Earlier ones are dropped: the window only moves forward.
    pub(crate) fn since(&self, at: i64) -> usize {
        let mut calls = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        calls.retain(|t| *t >= at);
        calls.len()
    }
}

tokio::task_local! {
    /// Set around a call whose decision row the log holds, so the meter does not count it again.
    static LOGGED: ();
}

/// Questions whose calls ride on a writer unit, which the queue already paces: the after-write
/// checks and the gate before a pursuit is written.
const PACED_BY_A_WRITER: [&str; 3] = ["complete", "adds_nothing", "answerable"];

/// Whether a request is judging the tick pays for: every question in it is a built-in one and
/// none rides on a writer unit. A person's own questions (the Decide verb) never are. A built-in
/// question is known by its key, or by its wording where a swapped read renamed the keys.
fn spends_the_tick(questions: &[Question]) -> bool {
    let builtin = catalogue::all();
    !questions.is_empty()
        && questions.iter().all(|q| {
            builtin.iter().any(|a| {
                !PACED_BY_A_WRITER.contains(&a.id)
                    && (q.key == a.question.key || q.instructions == a.question.instructions)
            })
        })
}

/// The decider a `Core` is built with: it counts the judging calls that write no decision row.
pub struct Metered {
    inner: Arc<dyn Decider>,
    calls: Arc<Meter>,
}

impl Metered {
    pub fn new(inner: Arc<dyn Decider>, calls: Arc<Meter>) -> Self {
        Self { inner, calls }
    }
}

#[async_trait]
impl Decider for Metered {
    async fn decide(&self, state: &str, questions: &[Question]) -> Result<Answers> {
        // Counted when sent, answered or not: a call that fails held the server too.
        if LOGGED.try_with(|_| ()).is_err() && spends_the_tick(questions) {
            self.calls.record(crate::store::now());
        }
        self.inner.decide(state, questions).await
    }

    fn context_tokens(&self) -> usize {
        self.inner.context_tokens()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }
}

/// Queued label units whose candidates are counted; more than a tick could ever hold.
const QUEUED_LABELS: i64 = 200;

impl Core {
    /// How many decision calls an arming sweep may still arm in this tick: the cap, less the calls
    /// already sent in it (logged decisions and the meter's) and the calls queued units will send.
    pub async fn judging_budget(&self) -> Result<usize> {
        let since = crate::store::now() - i64::from(self.decide_cfg.tick_mins) * 60;
        let logged = self.store.decided_since(since).await?.max(0) as usize;
        let spent = logged.saturating_add(self.calls.since(since));
        let left = self.decide_cfg.max_per_tick.saturating_sub(spent);
        if left == 0 {
            return Ok(0);
        }
        Ok(left.saturating_sub(self.judging_queued(left).await?))
    }

    /// The calls queued judging units will still send, counted no further than `upto`: one for a
    /// pair, a link or a note, one per live candidate for a search. An armed unit holds its share
    /// from the moment it is armed, so two sweeps in one tick cannot both spend it.
    async fn judging_queued(&self, upto: usize) -> Result<usize> {
        use crate::store::jobs::Stage;
        let mut n = 0usize;
        for stage in [Stage::Dedupe, Stage::LinkJudge, Stage::Read] {
            n = n.saturating_add(self.store.queued_units(stage).await?.max(0) as usize);
        }
        for id in self
            .store
            .queued_unit_targets(Stage::Label, QUEUED_LABELS)
            .await?
        {
            if n >= upto {
                break;
            }
            let Some((_, candidates)) = self.store.search_to_label(&id).await? else {
                continue;
            };
            let live = self
                .store
                .artifacts_by_ids(&candidates)
                .await?
                .iter()
                .filter(|c| c.in_results())
                .count();
            n = n.saturating_add(live.max(1));
        }
        Ok(n)
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

/// The reason an act carries between `ask` and its stage's `settle` or `demote_to_ask`.
const UNSETTLED: &str = "unsettled";

/// What `answer` and `ask_again` say when a press comes too late; a page turns these into a notice.
pub const ALREADY_ANSWERED: &str = "this was already answered";
pub const CHANGED_SINCE_ASKED: &str = "this changed since it was asked; it will be asked again";
pub const NOT_ASKED_AGAIN: &str = "this cannot be asked again";

async fn logged(core: &Core, id: &str) -> Result<Logged> {
    core.store.logged(id).await?.ok_or(Error::NotFound)
}

/// Probability by pick, named as `pick_and_top` names picks; no text of the model's is kept.
pub(crate) fn weights(q: &Question, a: &Answer) -> serde_json::Value {
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
    serde_json::Value::Object(map)
}

/// Put the question, band it, log it. `state` is the whole state; `None` means the subjects do not fit:
/// logged as leave with reason "too_long" and no call.
pub async fn ask(core: &Core, a: &Asked, s: Subjects, state: Option<&str>) -> Result<Logged> {
    ask_with(core, a, &[], s, state).await
}

/// `ask`, with more questions in the same request, read as `a` is (swapped or not). The decision is
/// `a`'s; each other read is kept in its answer JSON under its name, as `pick`, `top` and `agree`.
pub async fn ask_with(
    core: &Core,
    a: &Asked,
    with: &[(&str, &Asked)],
    s: Subjects,
    state: Option<&str>,
) -> Result<Logged> {
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
        let mut questions = vec![a.question.clone()];
        questions.extend(with.iter().map(|(_, w)| w.question.clone()));
        let key = &a.question.key;
        // The light turn: a decision is short and does not serve or start the cooldown.
        let permit = core.gate.background_light().await;
        // The row logged below is what the tick counts for this call.
        let got = LOGGED
            .scope((), async {
                if a.swapped {
                    decide_swapped(core.decider.as_ref(), state, &questions)
                        .await
                        .map(|s| (s.answers, s.agree))
                } else {
                    core.decider
                        .decide(state, &questions)
                        .await
                        .map(|a| (a, HashMap::new()))
                }
            })
            .await;
        permit.finished();
        let (answers, agreed) = got?;
        let read = |q: &Question| {
            answers
                .answers
                .get(&q.key)
                .ok_or_else(|| Error::InferenceRejected {
                    role: "decide",
                    detail: format!("the answer has none for \"{}\"", q.key),
                })
        };
        let answer = read(&a.question)?;
        let agree = agreed.get(key).copied();
        let (pick, top) = pick_and_top(&a.question, answer);
        let b = band(top, agree, core.thresholds(a).await?);
        let mut json = weights(&a.question, answer);
        for (name, w) in with {
            let (pick, top) = pick_and_top(&w.question, read(&w.question)?);
            json[*name] = serde_json::json!({
                "pick": pick,
                "top": top,
                "agree": agreed.get(&w.question.key),
            });
        }
        row.answer = json.to_string();
        row.top = top;
        row.pick = pick;
        row.swapped_agree = agree;
        row.band = b;
        // Hidden from the sweep until the stage that asked settles or demotes it.
        row.reason = (b == Band::Act).then(|| UNSETTLED.into());
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
            // Settled whatever happens next: a waiting act is the sweep's from here.
            core.store.mark_act_settled(&d.id).await?;
            let Some(applier) = core.appliers.get(&d.question_id) else {
                return Ok((logged(core, &d.id).await?, false));
            };
            if !applier.armed(core) {
                return Ok((logged(core, &d.id).await?, false));
            }
            if let Some(job) = applier.job()
                && !core.may_act(job).await?
            {
                return Ok((logged(core, &d.id).await?, false));
            }
            // Claimed before the act, so a second worker, or a demotion since `d` was read, wins.
            if !core.store.claim_act(&d.id).await? {
                return Ok((logged(core, &d.id).await?, false));
            }
            match applier.apply(core, &d, &d.pick, DecidedBy::Model).await {
                Ok(rows) => core.store.link_decision_actions(&d.id, &rows).await?,
                Err(e) => {
                    release(core, &d.id, State::Waiting).await;
                    return Err(e);
                }
            }
            // An applier that has to wait gives its claim back: applied only if it stayed done.
            let after = logged(core, &d.id).await?;
            let acted = after.state == State::Done;
            Ok((after, acted))
        }
        Band::Ask => {
            let max = i64::try_from(core.decide_cfg.ask_open_max).unwrap_or(i64::MAX);
            let seated = core.store.seat_ask(&d.id, max).await?;
            // As it stands: a sweep may have seated it since `d` was read.
            Ok((logged(core, &d.id).await?, seated))
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
    let answered = || Error::Validation(ALREADY_ANSWERED.into());
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
    // Before the buttons: they are drawn from the subjects, and these may be gone.
    if !applier.unchanged(core, &d).await? {
        return Err(match reask_changed(core, applier.as_ref(), &d).await? {
            true => Error::Validation(CHANGED_SINCE_ASKED.into()),
            false => answered(),
        });
    }
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
    // On the row first: a unit `apply` arms can run before the outcome below is written.
    core.store.set_answered_pick(id, Some(pick)).await?;
    // A person's pick is not subject to the daily cap.
    match applier.apply(core, &d, pick, DecidedBy::Operator).await {
        Ok(rows) => core.store.link_decision_actions(id, &rows).await?,
        Err(e) => {
            if let Err(undo) = core.store.set_answered_pick(id, None).await {
                tracing::warn!(decision = id, error = %undo, "could not take a failed pick off its row");
            }
            release(core, id, State::Open).await;
            return Err(e);
        }
    }
    let outcome = if applier.agrees(&d, pick) {
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
    let applier = core
        .appliers
        .get(&d.question_id)
        .ok_or_else(|| Error::Validation("nothing here can ask this again".into()))?;
    // Taken off the list before the subject is put back, so of two presses one does it.
    if !core.store.claim_ask_again(id).await? {
        return Err(Error::Validation(NOT_ASKED_AGAIN.into()));
    }
    if let Err(e) = applier.reopen(core, &d).await {
        if let Err(undo) = core.store.release_ask_again(id, d.reason.as_deref()).await {
            tracing::warn!(decision = id, error = %undo, "could not list an answer again after its subject would not reopen");
        }
        return Err(e);
    }
    Ok(())
}

/// Close an ask whose subjects are no longer what was asked about and put them back to be asked
/// again. False: someone closed the row first. A subject that cannot be put back leaves the row
/// as it was, so the next sweep tries again.
pub(crate) async fn reask_changed(core: &Core, applier: &dyn Applier, d: &Logged) -> Result<bool> {
    if !core.store.close_changed_ask(&d.id).await? {
        return Ok(false);
    }
    if let Err(e) = applier.reopen(core, d).await {
        if let Err(undo) = core
            .store
            .restore_changed_ask(&d.id, d.state, d.reason.as_deref())
            .await
        {
            tracing::warn!(decision = %d.id, error = %undo, "could not put back an ask whose subject would not reopen");
        }
        return Err(e);
    }
    Ok(true)
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
        // runtime: test-only backdating, of when it was decided and when it was seated.
        sqlx::query(
            "UPDATE decisions SET decided_at = decided_at - ?1, opened_at = opened_at - ?1
              WHERE id = ?2",
        )
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
    async fn a_sweep_does_not_see_an_unsettled_act() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        assert!(core.store.waiting_acts(10).await.unwrap().is_empty());
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.applied, r.reasked), (0, 0));
        assert!(ap.applied().is_empty());
        assert_eq!(state_of(&core, &d.id).await, State::Waiting);
        // Settled, it is the sweep's to apply.
        assert_eq!(settle(&core, d).await.unwrap().state, State::Done);
    }

    #[tokio::test]
    async fn a_demote_racing_a_sweep_never_applies() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.99"))
            .await
            .unwrap();
        let (r, demoted) = tokio::join!(
            crate::jobs::decisions::run(&core),
            demote_to_ask(&core, d, "restricted")
        );
        assert_eq!(r.unwrap().applied, 0);
        let d = settle(&core, demoted.unwrap()).await.unwrap();
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        assert!(ap.applied().is_empty());
    }

    #[tokio::test]
    async fn an_act_read_before_its_demotion_is_not_claimed() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.99"))
            .await
            .unwrap();
        settle(&core, d).await.unwrap();
        let stale = core.store.waiting_acts(10).await.unwrap().remove(0);
        demote_to_ask(&core, stale.clone(), "restricted")
            .await
            .unwrap();
        core.evolve.max_actions_per_day = 20;
        settle(&core, stale.clone()).await.unwrap();
        assert!(ap.applied().is_empty());
        let stored = core.store.logged(&stale.id).await.unwrap().unwrap();
        assert_eq!((stored.band, stored.state), (Band::Ask, State::Waiting));
    }

    #[tokio::test]
    async fn an_orphaned_unsettled_act_is_closed_and_asked_again_not_applied() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let young = ask(&core, &q_choice(), subjects("young"), Some("0.95"))
            .await
            .unwrap();
        let orphan = ask(&core, &q_choice(), subjects("orphan"), Some("0.95"))
            .await
            .unwrap();
        backdate(&core, &orphan.id, 16 * 60).await;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.reasked, r.applied), (1, 0));
        assert_eq!(ap.reopened(), vec![orphan.id.clone()]);
        assert!(ap.applied().is_empty());
        let stored = core.store.logged(&orphan.id).await.unwrap().unwrap();
        assert_eq!(stored.state, State::Done);
        assert_eq!(stored.reason.as_deref(), Some("changed"));
        assert_eq!(state_of(&core, &young.id).await, State::Waiting);
        // The stage that asked cannot apply it afterwards either.
        settle(&core, orphan).await.unwrap();
        assert!(ap.applied().is_empty());
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
        // The two that wait: the lower top was asked first, so insertion order would seat it.
        for p in ["0.7", "0.8", "0.65", "0.75"] {
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
            [State::Done, State::Open, State::Waiting, State::Open],
            "the seat went to the higher top, asked last"
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
    async fn two_presses_of_ask_again_put_the_subject_back_once() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        ap.set_journals(false);
        let bare = open_row(&core, "bare").await;
        answer(&core, &bare, Some("no")).await.unwrap();
        let (one, two) = tokio::join!(ask_again(&core, &bare), ask_again(&core, &bare));
        assert_eq!(
            [one.is_ok(), two.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "one press wins, the other is refused"
        );
        assert_eq!(ap.reopened(), vec![bare.clone()]);
        assert_eq!(
            core.store
                .logged(&bare)
                .await
                .unwrap()
                .unwrap()
                .reason
                .as_deref(),
            Some("asked_again")
        );
    }

    #[tokio::test]
    async fn an_answer_that_will_not_reopen_stays_listed_for_asking_again() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        ap.set_journals(false);
        let bare = open_row(&core, "bare").await;
        answer(&core, &bare, Some("no")).await.unwrap();
        ap.set_reopen_failing(true);
        assert!(ask_again(&core, &bare).await.is_err());
        let listed = core.store.answered_without_action(10).await.unwrap();
        assert_eq!(listed.len(), 1, "the claim was not taken back");
        assert_eq!(listed[0].reason, None);
        ap.set_reopen_failing(false);
        ask_again(&core, &bare).await.unwrap();
        assert_eq!(ap.reopened(), vec![bare]);
    }

    #[tokio::test]
    async fn an_answer_about_something_that_changed_is_not_carried_out_and_is_asked_again() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        ap.set_changed(true);
        let Err(Error::Validation(m)) = answer(&core, &id, Some("yes")).await else {
            panic!("an answer about a changed subject went through");
        };
        assert_eq!(m, CHANGED_SINCE_ASKED);
        assert!(ap.applied().is_empty());
        assert_eq!(ap.reopened(), vec![id.clone()]);
        let d = core.store.logged(&id).await.unwrap().unwrap();
        assert_eq!(
            (d.state, d.reason.as_deref(), d.outcome),
            (State::Done, Some("changed"), None)
        );
        assert_eq!(core.store.open_ask_count().await.unwrap(), 0);
        // Leave asks nothing of the subjects.
        let left = open_row(&core, "b").await;
        let d = answer(&core, &left, None).await.unwrap();
        assert_eq!((d.state, d.reason), (State::Done, None));
        assert_eq!(ap.reopened().len(), 1);
    }

    #[tokio::test]
    async fn an_open_row_whose_subject_changed_is_closed_asked_again_and_gives_up_its_seat() {
        let (mut core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.decide_cfg.ask_open_max = 1;
        let open = open_row(&core, "0.8").await;
        let waiting = ask(&core, &q_noul(), subjects("0.7"), Some("0.7"))
            .await
            .unwrap();
        let waiting = settle(&core, waiting).await.unwrap();
        assert_eq!(waiting.state, State::Waiting, "no seat for it");
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.reasked, r.seated), (0, 0), "nothing changed yet");

        ap.set_changed(true);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        // Both are about something that changed: the seated one is closed, the other never seated.
        assert_eq!((r.reasked, r.seated), (2, 0));
        assert_eq!(ap.reopened().len(), 2);
        for id in [&open, &waiting.id] {
            let d = core.store.logged(id).await.unwrap().unwrap();
            assert_eq!(
                (d.state, d.reason.as_deref(), d.outcome),
                (State::Done, Some("changed"), None)
            );
        }
        assert_eq!(core.store.open_ask_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_seat_freed_by_a_changed_row_is_taken_in_the_same_sweep() {
        let (mut core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.decide_cfg.ask_open_max = 1;
        let open = open_row(&core, "seated").await;
        let waiting = ask(&core, &q_noul(), subjects("waits"), Some("0.7"))
            .await
            .unwrap();
        let waiting = settle(&core, waiting).await.unwrap();
        assert_eq!(waiting.state, State::Waiting, "no seat for it");
        ap.set_changed_subject("seated");
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.reasked, r.seated), (1, 1));
        assert_eq!(ap.reopened(), vec![open.clone()]);
        assert_eq!(state_of(&core, &open).await, State::Done);
        assert_eq!(state_of(&core, &waiting.id).await, State::Open);
    }

    #[tokio::test]
    async fn an_ask_whose_subject_will_not_reopen_stays_as_it_was_for_the_next_sweep() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        ap.set_changed(true);
        ap.set_reopen_failing(true);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.reasked, 0);
        let d = core.store.logged(&id).await.unwrap().unwrap();
        assert_eq!((d.state, d.reason), (State::Open, None));
        ap.set_reopen_failing(false);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.reasked, 1);
        assert_eq!(ap.reopened(), vec![id]);
    }

    #[tokio::test]
    async fn an_answer_the_applier_calls_agreeing_is_confirmed_under_another_name() {
        let ap = TestApplier::new(NOUL)
            .with_buttons(&[("keep_one", "Keep one"), ("no", "Drop")])
            .agreeing_on("keep_one");
        let (core, ap, _) = core_with(ap, by_state).await;
        let id = open_row(&core, "a").await;
        let d = answer(&core, &id, Some("keep_one")).await.unwrap();
        assert_eq!(d.pick, "yes", "the model's pick is not a button here");
        assert_eq!(d.outcome, Some(Outcome::Confirmed));
        assert_eq!(d.answered_pick.as_deref(), Some("keep_one"));
        assert_eq!(
            ap.applied(),
            vec![("keep_one".to_string(), DecidedBy::Operator)]
        );
        let other = open_row(&core, "b").await;
        let d = answer(&core, &other, Some("no")).await.unwrap();
        assert_eq!(d.outcome, Some(Outcome::Corrected));
    }

    #[tokio::test]
    async fn a_disarmed_applier_leaves_a_sure_act_waiting_through_settle_and_the_sweep() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        ap.set_armed(false);
        let d = ask(&core, &q_choice(), subjects("a"), Some("0.95"))
            .await
            .unwrap();
        let d = settle(&core, d).await.unwrap();
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
        assert_eq!(d.reason, None, "settled: the sweep's from here");
        for _ in 0..2 {
            let r = crate::jobs::decisions::run(&core).await.unwrap();
            assert_eq!((r.applied, r.reasked, r.seated), (0, 0, 0));
        }
        assert!(ap.applied().is_empty());
        assert_eq!(state_of(&core, &d.id).await, State::Waiting);
        ap.set_armed(true);
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(ap.applied(), vec![("same".to_string(), DecidedBy::Model)]);
    }

    #[tokio::test]
    async fn acts_of_a_disarmed_question_do_not_fill_the_sweep_ahead_of_ones_that_can_act() {
        let (mut core, held, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        held.set_armed(false);
        let free = Arc::new(TestApplier::new(NOUL));
        let mut appliers = Appliers::default();
        appliers.register(held.clone());
        appliers.register(free.clone());
        core.appliers = Arc::new(appliers);
        // More than one sweep takes, every one of them above the act that can be carried out.
        for i in 0..201 {
            seeded(
                &core,
                CHOICE,
                &format!("h{i}"),
                serde_json::json!({"same": 0.99, "different": 0.01}),
                Band::Act,
            )
            .await;
        }
        seeded(
            &core,
            NOUL,
            "f",
            serde_json::json!({"yes": 0.91, "no": 0.09}),
            Band::Act,
        )
        .await;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(free.applied(), vec![("yes".to_string(), DecidedBy::Model)]);
        assert!(held.applied().is_empty());
    }

    #[tokio::test]
    async fn a_persons_pick_is_on_the_row_while_it_is_carried_out_and_off_it_when_that_fails() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        ap.set_failing(true);
        assert!(answer(&core, &id, Some("yes")).await.is_err());
        let d = core.store.logged(&id).await.unwrap().unwrap();
        assert_eq!((d.state, d.answered_pick), (State::Open, None));
        ap.set_failing(false);
        answer(&core, &id, Some("yes")).await.unwrap();
        assert_eq!(
            ap.picks_seen(),
            vec![Some("yes".to_string()), Some("yes".to_string())],
            "what carrying it out arms must find whose ask it is"
        );
    }

    #[tokio::test]
    async fn a_persons_answer_is_not_held_by_a_disarmed_applier() {
        let (core, ap, _) = core_with(TestApplier::new(NOUL), by_state).await;
        let id = open_row(&core, "a").await;
        ap.set_armed(false);
        let d = answer(&core, &id, Some("yes")).await.unwrap();
        assert_eq!(d.state, State::Done);
        assert_eq!(ap.applied(), vec![("yes".to_string(), DecidedBy::Operator)]);
    }

    /// A waiting act of a question nothing here answers, settled or not.
    async fn strayed(core: &crate::core::Core, a: &str, settled: bool) -> String {
        core.store
            .log_decision(&NewLogged {
                question_id: "gone_question".into(),
                question_version: 1,
                model: core.decider.model().into(),
                subject_kind: "pair",
                subject_a: a.into(),
                subject_b: None,
                rev_a: None,
                rev_b: None,
                answer: "{}".into(),
                top: 0.99,
                pick: "yes".into(),
                swapped_agree: None,
                band: Band::Act,
                reason: (!settled).then(|| "unsettled".into()),
                state: State::Waiting,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_waiting_act_no_applier_answers_is_closed_and_holds_no_place_in_the_sweep() {
        let (core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        let orphan = strayed(&core, "orphan", false).await;
        backdate(&core, &orphan, 16 * 60).await;
        let young = strayed(&core, "young", false).await;
        let settled = strayed(&core, "settled", true).await;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.applied, r.reasked), (0, 0));
        for id in [&orphan, &settled] {
            let d = core.store.logged(id).await.unwrap().unwrap();
            assert_eq!(
                (d.state, d.reason.as_deref()),
                (State::Done, Some("no_applier"))
            );
        }
        // Not yet an orphan: the stage that asked may still settle it.
        assert_eq!(state_of(&core, &young).await, State::Waiting);
        assert!(core.store.waiting_acts(10).await.unwrap().is_empty());
        assert!(ap.applied().is_empty() && ap.reopened().is_empty());
    }

    #[tokio::test]
    async fn a_store_error_on_one_decision_does_not_stop_the_sweep() {
        let (mut core, ap, _) = core_with(TestApplier::new(CHOICE), by_state).await;
        core.evolve.max_actions_per_day = 0;
        let mut ids = vec![];
        for a in ["stuck", "free"] {
            let d = ask(&core, &q_choice(), subjects(a), Some("0.95"))
                .await
                .unwrap();
            ids.push(settle(&core, d).await.unwrap().id);
        }
        let orphan = ask(&core, &q_choice(), subjects("stuck"), Some("0.95"))
            .await
            .unwrap();
        backdate(&core, &orphan.id, 16 * 60).await;
        let late = ask(&core, &q_choice(), subjects("free"), Some("0.95"))
            .await
            .unwrap();
        backdate(&core, &late.id, 16 * 60).await;
        // runtime: test-only, every write to the decisions about "stuck" fails.
        sqlx::query(
            "CREATE TRIGGER stuck BEFORE UPDATE ON decisions WHEN OLD.subject_a = 'stuck'
             BEGIN SELECT RAISE(ABORT, 'stuck'); END",
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        ap.set_changed(true);
        core.evolve.max_actions_per_day = 20;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.reasked, 2, "the two that could be written were");
        let mut reopened = ap.reopened();
        reopened.sort();
        let mut want = vec![ids[1].clone(), late.id.clone()];
        want.sort();
        assert_eq!(reopened, want);
        for id in [&ids[0], &orphan.id] {
            assert_eq!(state_of(&core, id).await, State::Waiting);
        }
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
        let old = ask(&core, &q_noul(), subjects("old"), Some("0.5"))
            .await
            .unwrap();
        backdate(&core, &old.id, 16 * 60).await;
        for a in ["a", "b"] {
            ask(&core, &q_noul(), subjects(a), Some("0.5"))
                .await
                .unwrap();
        }
        assert_eq!(core.judging_budget().await.unwrap(), 1);
        // Too long to ask about: logged, and no call was made.
        ask(&core, &q_noul(), subjects("long"), None).await.unwrap();
        assert_eq!(core.judging_budget().await.unwrap(), 1);
        for a in ["c", "d"] {
            ask(&core, &q_noul(), subjects(a), Some("0.5"))
                .await
                .unwrap();
        }
        assert_eq!(core.judging_budget().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_queued_unit_holds_its_share_of_the_tick_until_it_reaches_the_retry_ceiling() {
        let (mut core, _, _) = core_with(TestApplier::new(NOUL), by_state).await;
        core.decide_cfg.max_per_tick = 5;
        use crate::store::jobs::Stage;
        for (stage, kind, target) in [
            (Stage::Read, "artifact", "n1"),
            (Stage::Read, "artifact", "n2"),
            (Stage::Dedupe, "pair", "7"),
        ] {
            core.store
                .rearm_idle_seq(stage, kind, target, 0)
                .await
                .unwrap();
        }
        assert_eq!(
            core.judging_budget().await.unwrap(),
            2,
            "three units armed and not yet run"
        );
        // runtime: test-only, a unit that has failed as often as the queue counts.
        sqlx::query("UPDATE jobs SET attempts = ? WHERE stage = 'read' AND target_id = 'n1'")
            .bind(crate::store::jobs::MAX_ATTEMPTS)
            .execute(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(
            core.judging_budget().await.unwrap(),
            3,
            "a unit at the ceiling holds nothing"
        );
    }

    #[tokio::test]
    async fn a_call_that_logs_no_decision_spends_the_tick_and_a_logged_one_spends_it_once() {
        use crate::infer::decide::decide_swapped;
        let (mut core, _, dec) = core_with(TestApplier::new(NOUL), by_state).await;
        metered(&mut core);
        core.decide_cfg.max_per_tick = 10;
        // A reading, and a link's exchanged read with its keys renamed by the swap.
        core.decider
            .decide("0.9", &[catalogue::pinned().question])
            .await
            .unwrap();
        decide_swapped(
            core.decider.as_ref(),
            "0.9",
            &[catalogue::link_direction().question],
        )
        .await
        .unwrap();
        assert_eq!(core.judging_budget().await.unwrap(), 8);
        // A decision: its row is the count, the meter adds nothing.
        ask(&core, &catalogue::reap_worth(), subjects("a"), Some("0.5"))
            .await
            .unwrap();
        assert_eq!(core.judging_budget().await.unwrap(), 7);
        assert_eq!(dec.calls(), 3);
        // Sent before this tick: out of the window.
        core.calls.record(crate::store::now() - 16 * 60);
        assert_eq!(core.judging_budget().await.unwrap(), 7);
    }

    #[tokio::test]
    async fn a_check_the_pursuit_gate_and_a_persons_question_do_not_spend_the_tick() {
        let (mut core, _, dec) = core_with(TestApplier::new(NOUL), by_state).await;
        metered(&mut core);
        let full = core.decide_cfg.max_per_tick;
        for questions in [
            vec![
                catalogue::complete().question,
                catalogue::adds_nothing().question,
            ],
            vec![catalogue::answerable().question],
            vec![Question::noul("verdict", "Keep bge-m3?")],
        ] {
            core.decider.decide("0.9", &questions).await.unwrap();
        }
        assert_eq!(dec.calls(), 3);
        assert_eq!(core.judging_budget().await.unwrap(), full);
    }

    #[test]
    fn the_reversed_read_is_told_by_the_key_the_swap_gives_it() {
        let wire = crate::infer::decide::swapped_questions(&[q_choice().question]);
        let keys: Vec<&str> = wire.iter().map(|q| q.key.as_str()).collect();
        assert_eq!(keys.len(), 2);
        assert!(!reversed_twin(keys[0]), "{keys:?}");
        assert!(reversed_twin(keys[1]), "{keys:?}");
        // A caller's own key that happens to end in r is not a twin.
        assert!(!reversed_twin("keeper"));
        assert!(!reversed_twin("qr"));
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
