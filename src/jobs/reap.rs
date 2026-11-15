//! Revisit the long-retired: `reap_worth` through the judgement layer. `ReapApplier` buries the
//! worthless and asks `Stage::Rescue` to rewrite the valuable live. A wrong burial answers to its
//! journal row, the Reaped listing's Restore, and `retract`'s exhumation.

use crate::core::Core;
use crate::core::judging::band::{Band, era};
use crate::core::judging::{self, Applier, Subjects, catalogue, state};
use crate::error::{Error, Result};
use crate::store::actions::{Job, Kind, NewAction};
use crate::store::artifacts::Chunk;
use crate::store::decision_log::{Logged, State};
use crate::store::jobs::Stage;
use crate::store::pairs::DecidedBy;
use async_trait::async_trait;

/// Flat numbers: `jobs::did_work` reads any non-zero flat count as work, and each count here is
/// this pass acting.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub judged: u64,
    pub reaped: u64,
    /// Rescue units armed.
    pub rescued: u64,
    /// Retired rows given a `retired_at` because they predate the column.
    pub stamped: u64,
}

pub(crate) fn reap_era(core: &Core) -> String {
    era(core.decider.model(), catalogue::reap_worth().version)
}

pub async fn run(core: &Core) -> Result<Report> {
    // Re-checked here: `periodic_units` gates arming, and nothing disarms a row already pending.
    if !core.reap.enabled {
        return Ok(Report::default());
    }
    let mut report = Report::default();
    let (cands, stamped) = nominees(core).await?;
    report.stamped = stamped;
    let asked = catalogue::reap_worth();
    // Inside the tick's budget. The daily cap is not checked here: past it a burial waits.
    let mut budget = core.judging_budget().await?;
    for c in &cands {
        if budget == 0 {
            tracing::info!("the tick's judging is spent; the rest of the nominees wait");
            break;
        }
        // A burial taken back is not bought again; checked before the call.
        if core.store.action_was_undone(&c.id, Kind::Reap).await? {
            tracing::debug!(artifact_id = %c.id, "a burial taken back before; left alone");
            // Stood down, not skipped: `reap_candidates` orders by `retired_at ASC`, so an
            // unstamped row would hold a nominee slot forever.
            step_aside(core, &c.id).await;
            continue;
        }
        let s = match state_of(core, c, &asked).await {
            Ok(s) => s,
            // One note's neighbours unread is not the sweep's failure: it waits, the rest go on.
            Err(e) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "could not read a candidate's neighbours; it waits");
                continue;
            }
        };
        let subjects = Subjects {
            kind: "artifact",
            a: c.id.clone(),
            b: c.superseded_by.clone(),
            rev_a: Some(c.embed_rev),
            rev_b: None,
        };
        budget -= 1;
        let d = match judging::ask(core, &asked, subjects, s.as_deref()).await {
            Ok(d) => d,
            // A rejection is about this row and stands it down; an outage is a wait and must not.
            Err(e @ Error::InferenceRejected { .. }) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "the judge will not rule on this candidate; standing it down");
                step_aside(core, &c.id).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "reap judgement failed; candidate waits");
                continue;
            }
        };
        // Counted only when a judgement came back, or a sweep with a dead endpoint looks busy and
        // never backs off.
        if s.is_some() {
            report.judged += 1;
        }
        let d = match judging::settle(core, d).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(artifact_id = %c.id, error = %e, "a reap decision could not be carried out; it waits");
                continue;
            }
        };
        if d.band == Band::Act && d.state == State::Done {
            if core.store.get_artifact(&c.id).await?.reaped_at.is_some() {
                report.reaped += 1;
            } else if core.store.live_job(Stage::Rescue, &c.id).await? {
                report.rescued += 1;
            }
        }
    }
    Ok(report)
}

fn note<'a>(title: Option<&'a str>, text: &'a str) -> state::Note<'a> {
    state::Note {
        title,
        day: None,
        text,
    }
}

/// The retired note, its replacement and its nearest live notes; `None` when even the note alone
/// does not fit.
async fn state_of(core: &Core, c: &Chunk, asked: &judging::Asked) -> Result<Option<String>> {
    let successor = match &c.superseded_by {
        Some(id) => core.store.get_artifact(id).await.ok(),
        None => None,
    };
    let neighbours = live_neighbours(core, &c.id).await?;
    let live: Vec<state::Note> = neighbours.iter().map(|(t, x)| note(Some(t), x)).collect();
    let successor = successor
        .as_ref()
        .map(|s| note(s.title.as_deref(), &s.text));
    Ok(state::reap(
        &note(c.title.as_deref(), &c.text),
        successor.as_ref(),
        &live,
        &core.counter,
        state::room(core, &[asked]),
    ))
}

/// Moves a candidate the sweep could not act on to the back of the queue with a fresh `retired_at`,
/// so one that fails the same way every time cannot starve the rest.
pub(crate) async fn step_aside(core: &Core, id: &str) {
    if let Err(e) = core.store.restamp_retired(id).await {
        tracing::warn!(artifact_id = %id, error = %e, "could not restamp a candidate the sweep could not act on");
    }
}

/// The nearest live artifacts, `(title, text)`. Errors propagate: a failed search read as "nothing
/// near" would let the judge bury text with no evidence in front of it.
pub(crate) async fn live_neighbours(core: &Core, id: &str) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for h in core.vectors.neighbours(id, 6).await? {
        if h.payload.artifact_id == id {
            continue;
        }
        if core
            .store
            .artifact_in_results(&h.payload.artifact_id)
            .await?
            == Some(true)
        {
            out.push((
                h.payload.title.clone().unwrap_or_else(|| "untitled".into()),
                h.payload.text.clone(),
            ));
        }
        if out.len() >= 5 {
            break;
        }
    }
    Ok(out)
}

/// Row before point, marker before delete: a failed delete leaves `lifecycle_dirty` for
/// `repair_lifecycle_drift`. Under `lifecycle_lock` so no restore lands between copy and wipe.
/// Nothing after `bury` commits is an error: an `Err` would put a burial that stands back to wait.
pub(crate) async fn reap_one(
    core: &Core,
    c: &Chunk,
    decision_id: &str,
    by: DecidedBy,
) -> Result<Vec<String>> {
    let meta = serde_json::json!({
        "status": c.status.as_str(),
        "superseded_by": c.superseded_by,
        "provenance": c.provenance.as_str(),
        "tags": c.tags,
        "corpus_id": c.corpus_id,
        "corpus_span": c.corpus_span,
        "created_at": c.created_at,
        "retired_at": c.retired_at,
    })
    .to_string();
    // Read before the point is deleted. A store that cannot answer buries without it, and says so.
    let dense = match core.vectors.dense_of(&c.id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(artifact_id = %c.id, error = %e, "buried without its vector");
            None
        }
    };
    let mut evidence = serde_json::json!({
        "status": c.status.as_str(),
        "retired_at": c.retired_at,
        "created_at": c.created_at,
        "decision": decision_id,
    });
    if by == DecidedBy::Operator {
        evidence["asked_by"] = serde_json::json!("operator");
    }
    let journal = NewAction {
        job: Job::Reap,
        kind: Kind::Reap,
        subject_id: c.id.clone(),
        survivor_id: None,
        detail: None,
        evidence,
        pair_score: None,
    };
    let _guard = core.lifecycle_lock.lock().await;
    // `bury` sets `lifecycle_dirty` and writes the journal row in its own transaction.
    let row = match core
        .store
        .bury(
            &c.id,
            &meta,
            min_age_secs(core),
            dense.as_deref(),
            c.embed_model.as_deref(),
            &journal,
        )
        .await
    {
        Ok(row) => row,
        Err(Error::NotFound) => {
            tracing::info!(artifact_id = %c.id, "no longer a candidate when its burial came; left alone");
            return Ok(vec![]);
        }
        Err(e) => return Err(e),
    };
    // Linked as written, and again by the caller from what is returned.
    if let Err(e) = core
        .store
        .link_decision_actions(decision_id, std::slice::from_ref(&row))
        .await
    {
        tracing::warn!(artifact_id = %c.id, error = %e, "could not link a burial to its decision yet");
    }
    match core
        .vectors
        .delete_artifacts(std::slice::from_ref(&c.id))
        .await
    {
        Ok(()) => {
            if let Err(e) = core
                .store
                .clear_lifecycle_dirty(std::slice::from_ref(&c.id))
                .await
            {
                tracing::warn!(artifact_id = %c.id, error = %e, "could not clear a buried note's marker; the repair pass deletes its point again");
            }
        }
        Err(e) => {
            tracing::warn!(artifact_id = %c.id, error = %e, "buried, but its point is still in the vector store; the repair pass deletes it")
        }
    }
    tracing::info!(artifact_id = %c.id, "reaped a retired artifact");
    Ok(vec![row])
}

/// Whether the base may carry out a reap decision on its own: the switches `periodic_units` arms
/// the sweep on, read again at each act because a decision can wait across a change of them.
fn acts_alone(core: &Core) -> bool {
    core.reap.enabled && core.evolve.autonomous.acts_on_corpus()
}

/// Gives back the claim `settle` took and journals nothing: the model's act waits for the next
/// tick. A wait is not a failure, so it is not an `Err`.
async fn wait(core: &Core, d: &Logged) -> Result<Vec<String>> {
    core.store
        .set_decision_state(&d.id, State::Done, State::Waiting)
        .await?;
    Ok(vec![])
}

/// The model's rescues are paced by `max_rescues_per_run` in flight; one already armed for this
/// note takes no new room. A person's is not paced.
async fn rescue_has_room(core: &Core, id: &str) -> Result<bool> {
    if core.store.live_job(Stage::Rescue, id).await? {
        return Ok(true);
    }
    let live = core
        .store
        .control
        .live_count(&core.store.subject, Stage::Rescue)
        .await?;
    Ok(live < i64::try_from(core.reap.max_rescues_per_run).unwrap_or(i64::MAX))
}

/// Asks `Stage::Rescue` to write.
async fn arm_rescue(core: &Core, c: &Chunk) -> Result<()> {
    if core.generator.is_none() {
        tracing::info!(artifact_id = %c.id, "valuable, but no generator model to rewrite it with; nothing is armed");
        return Ok(());
    }
    // Behind capture work, as the merge is.
    core.store
        .rearm_idle_seq(
            Stage::Rescue,
            "artifact",
            &c.id,
            crate::jobs::consolidate::DEDUPE_SEQ_BASE,
        )
        .await
}

/// What `reap_worth`'s answers do: "no" buries, "yes" asks for a rescue.
pub struct ReapApplier;

async fn note_of(core: &Core, d: &Logged) -> Result<Option<Chunk>> {
    match core.store.get_artifact(&d.subject_a).await {
        Ok(c) => Ok(Some(c)),
        Err(Error::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

#[async_trait]
impl Applier for ReapApplier {
    fn question_id(&self) -> &'static str {
        "reap_worth"
    }

    fn label(&self) -> &'static str {
        "Worth keeping?"
    }

    fn job(&self) -> Option<Job> {
        Some(Job::Reap)
    }

    async fn buttons(&self, _: &Core, _: &Logged) -> Result<Vec<(String, String)>> {
        Ok([("yes", "Valuable"), ("no", "Worthless")]
            .iter()
            .map(|(p, l)| (p.to_string(), l.to_string()))
            .collect())
    }

    async fn subjects(&self, core: &Core, d: &Logged) -> Result<Vec<(String, String)>> {
        Ok(note_of(core, d)
            .await?
            .map(|c| (c.id.clone(), crate::web::ui::row_label(&c).text))
            .into_iter()
            .collect())
    }

    async fn unchanged(&self, core: &Core, d: &Logged) -> Result<bool> {
        let Some(c) = note_of(core, d).await? else {
            return Ok(false);
        };
        Ok(Some(c.embed_rev) == d.rev_a
            && c.superseded_by == d.subject_b
            && core.store.reapable(&c.id, min_age_secs(core)).await?)
    }

    async fn apply(
        &self,
        core: &Core,
        d: &Logged,
        pick: &str,
        by: DecidedBy,
    ) -> Result<Vec<String>> {
        let Some(c) = note_of(core, d).await? else {
            return Ok(vec![]);
        };
        // Again here, as `settle` reads `armed`. A person's answer is never held.
        if by == DecidedBy::Model && !acts_alone(core) {
            tracing::debug!(artifact_id = %c.id, "the reap is off or the base may not act alone; the decision waits");
            return wait(core, d).await;
        }
        match pick {
            "no" => {
                // Again here: a waiting burial can outlive a restore. A person's verdict wins.
                if by == DecidedBy::Model && core.store.action_was_undone(&c.id, Kind::Reap).await?
                {
                    tracing::info!(artifact_id = %c.id, "a burial taken back before; left alone");
                    return Ok(vec![]);
                }
                if by == DecidedBy::Model
                    && core
                        .store
                        .person_found_valuable(&c.id, c.superseded_by.as_deref(), c.embed_rev)
                        .await?
                {
                    tracing::info!(artifact_id = %c.id, "a person found this valuable; left alone");
                    return Ok(vec![]);
                }
                reap_one(core, &c, &d.id, by).await
            }
            "yes" => {
                // With no generator nothing is armed, so nothing is held.
                if by == DecidedBy::Model
                    && core.generator.is_some()
                    && !rescue_has_room(core, &c.id).await?
                {
                    tracing::debug!(artifact_id = %c.id, "as many rescues are armed as a run may write; this one waits");
                    return wait(core, d).await;
                }
                arm_rescue(core, &c).await.map(|()| vec![])
            }
            other => Err(Error::Validation(format!(
                "\"{other}\" is not an answer about a retired note"
            ))),
        }
    }

    /// Nothing to put back: the reason the caller stamps (`asked_again`, `changed`) is what lets
    /// `reap_candidates` nominate the note again, in its own place. A restamp would silence the
    /// question for `min_age_days`; a note buried, restored or rescued since is left as it is.
    async fn reopen(&self, _: &Core, _: &Logged) -> Result<()> {
        Ok(())
    }

    fn armed(&self, core: &Core) -> bool {
        acts_alone(core)
    }
}

/// Rows read per row to judge: the seen-since-retired filter runs on vector payloads after the page
/// is fetched.
const OVERFETCH: i64 = 4;

/// How long out of `active` before the sweep may look; `nominees`, `reap_one` and the rescue must
/// re-check against the same number.
pub(crate) fn min_age_secs(core: &Core) -> i64 {
    (core.reap.min_age_days as i64).saturating_mul(86_400)
}

/// Who goes before the judge: `reap_candidates` holds every SQL rule; the seen-since-retired rule
/// reads the vector point.
pub(crate) async fn nominees(core: &Core) -> Result<(Vec<Chunk>, u64)> {
    let stamped = core.store.stamp_unaged_retired().await?;
    if stamped > 0 {
        tracing::info!(stamped, "gave pre-column retirements a clock");
    }
    let min_age_secs = min_age_secs(core);
    let want = core.reap.max_judged_per_run as i64;
    let mut cands = core
        .store
        .reap_candidates(
            min_age_secs,
            &reap_era(core),
            want.saturating_mul(OVERFETCH),
        )
        .await?;
    if cands.is_empty() {
        return Ok((cands, stamped));
    }
    let ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
    let payloads = core.vectors.payloads_of(&ids).await?;
    cands.retain(|c| {
        goes_before_the_judge(
            c.retired_at,
            payloads.get(&c.id).and_then(|p| p.last_seen_at),
        )
    });
    // The cap is the sweep's: what is over it comes back oldest-first on the next run.
    cands.truncate(want.max(0) as usize);
    Ok((cands, stamped))
}

/// Only if nothing has reached it since retirement. A missing `retired_at` is treated as the
/// beginning of time, so any `last_seen_at` holds the row back.
fn goes_before_the_judge(retired_at: Option<i64>, last_seen_at: Option<i64>) -> bool {
    last_seen_at.is_none_or(|t| t < retired_at.unwrap_or(0))
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::core::Core;
    use crate::error::Result;
    use crate::infer::decide::{Answer, Answers, Question};
    use crate::infer::fake::RuleDecider;
    use crate::store::artifacts::{ArtifactStatus, NewArtifact};
    use std::sync::Arc;

    /// `reap_worth`'s "no" past the built-in line (0.99).
    pub(crate) const SURE_NO: f64 = 0.005;
    /// Its "yes" past the same line.
    pub(crate) const SURE_YES: f64 = 0.995;

    pub(crate) fn row(id: &str) -> crate::store::actions::NewAction {
        crate::store::actions::NewAction {
            job: crate::store::actions::Job::Reap,
            kind: crate::store::actions::Kind::Reap,
            subject_id: id.to_string(),
            survivor_id: None,
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: None,
        }
    }

    /// Every question answered "yes" with probability `yes`.
    pub(crate) fn answers(qs: &[Question], yes: f64) -> Result<Answers> {
        Ok(Answers {
            answers: qs
                .iter()
                .map(|q| (q.key.clone(), Answer::Noul { yes }))
                .collect(),
            model: "rule-decider".into(),
            input_tokens: 0,
        })
    }

    /// A decider answering `reap_worth` "yes" with probability `yes`, whatever the state.
    pub(crate) fn deciding(core: &mut Core, yes: f64) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(move |_, qs| answers(qs, yes)));
        core.decider = d.clone();
        d
    }

    /// A decider that answers each call with the next of `yes`, the last one for ever.
    pub(crate) fn scripted(core: &mut Core, yes: &[f64]) -> Arc<RuleDecider> {
        let left = std::sync::Mutex::new(yes.to_vec());
        let d = Arc::new(RuleDecider::new(move |_, qs| {
            let mut left = left.lock().unwrap();
            let p = if left.len() > 1 {
                left.remove(0)
            } else {
                left[0]
            };
            answers(qs, p)
        }));
        core.decider = d.clone();
        d
    }

    /// A decider that fails every call with `e`.
    pub(crate) fn failing(
        core: &mut Core,
        e: impl Fn() -> crate::error::Error + Send + Sync + 'static,
    ) -> Arc<RuleDecider> {
        let d = Arc::new(RuleDecider::new(move |_, _| Err(e())));
        core.decider = d.clone();
        d
    }

    pub(crate) async fn seed(core: &Core, texts: &[&str]) -> Vec<String> {
        let src = core
            .store
            .insert_corpus(&texts.join(" | "), "web", None)
            .await
            .unwrap();
        let rows: Vec<NewArtifact> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| NewArtifact {
                ordinal: i as i64,
                text: (*t).into(),
                title: Some(format!("S{i}")),
                ..Default::default()
            })
            .collect();
        core.store
            .insert_artifacts(&src.id, &rows)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    pub(crate) async fn backdate_retired_at(core: &Core, id: &str, secs_ago: i64) {
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            crate::store::now() - secs_ago,
            id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    pub(crate) async fn deprecate_long_ago(core: &Core, id: &str) {
        core.store
            .set_artifact_status(id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(core, id, 100 * 86_400).await;
    }

    /// The last `reap_worth` decision about `id` against `successor`.
    pub(crate) async fn decision(
        core: &Core,
        id: &str,
        successor: Option<&str>,
    ) -> crate::store::decision_log::Logged {
        core.store
            .last_logged("reap_worth", id, successor, &super::reap_era(core))
            .await
            .unwrap()
            .expect("the note was asked about")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::core::judging::Applier;
    use crate::core::judging::band::Band;
    use crate::core::test_support::test_core;
    use crate::store::decision_log::{Outcome, State};
    use crate::store::jobs::Stage;

    #[test]
    fn a_missing_retirement_stamp_defends_the_row() {
        assert!(goes_before_the_judge(Some(100), None));
        assert!(goes_before_the_judge(None, None));
        assert!(!goes_before_the_judge(Some(100), Some(100)));
        assert!(!goes_before_the_judge(Some(100), Some(200)));
        assert!(goes_before_the_judge(Some(100), Some(99)));
        assert!(!goes_before_the_judge(None, Some(1)));
        assert!(!goes_before_the_judge(None, Some(i64::MAX)));
    }

    use crate::store::artifacts::{ArtifactStatus, NewMerged};

    fn tenant(core: &Core) -> crate::tenants::Tenant {
        crate::tenants::Tenant {
            core: core.clone(),
            user: crate::store::control::User {
                subject: core.store.subject.clone(),
                email: None,
                slug: "u".into(),
                created_at: 0,
            },
        }
    }

    #[tokio::test]
    async fn a_burial_taken_back_stands_down_instead_of_holding_its_slot() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let mut core = test_core().await;
        core.reap.enabled = true;
        // A decider that never answers: every nominee fails at the call, below the branch under test.
        failing(&mut core, || crate::error::Error::Inference {
            role: "decide",
            detail: "connection refused".into(),
        });
        let ids = seed(&core, &["taken back before", "behind it"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        deprecate_long_ago(&core, &ids[1]).await;
        core.store
            .record_action(&NewAction {
                job: Job::Reap,
                kind: Kind::Reap,
                subject_id: ids[0].clone(),
                survivor_id: None,
                detail: None,
                evidence: serde_json::json!({}),
                pair_score: None,
            })
            .await
            .unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Reap, UndoneBy::Operator, "restored")
            .await
            .unwrap();
        let before = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        // Read back, not computed again: the clock is in seconds and may tick during the run.
        let waiting = core.store.get_artifact(&ids[1]).await.unwrap().retired_at;

        run(&core).await.unwrap();

        let after = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        assert!(
            after > before,
            "the row kept its place at the head of the queue: {before:?} -> {after:?}"
        );
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().retired_at,
            waiting,
            "a candidate the endpoint never answered for was stood down too"
        );
    }

    #[tokio::test]
    async fn an_active_young_seen_or_reminded_artifact_is_never_nominated() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &["active", "young", "seen", "unreachable-reminder", "clean"],
        )
        .await;
        core.store
            .set_artifact_status(&ids[1], ArtifactStatus::Deprecated)
            .await
            .unwrap(); // fresh stamp — too young
        deprecate_long_ago(&core, &ids[2]).await;
        crate::jobs::embed::run(&core, &ids[2]).await.unwrap();
        core.vectors
            .touch(&[crate::vector::Touch::shown(&ids[2])], crate::store::now())
            .await
            .unwrap(); // seen after retirement
        // A retired artifact is on no band, so its own reminder holds nothing back.
        deprecate_long_ago(&core, &ids[3]).await;
        open_reminder(&core, "m1", &ids[3]).await;
        deprecate_long_ago(&core, &ids[4]).await;

        let pair = seed(&core, &["live", "retired-beside-a-live-reminder"]).await;
        open_reminder(&core, "m2", &pair[0]).await;
        deprecate_long_ago(&core, &pair[1]).await;

        let (cands, _) = nominees(&core).await.unwrap();
        let mut got = cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>();
        got.sort_unstable();
        let mut want = vec![ids[3].as_str(), ids[4].as_str()];
        want.sort_unstable();
        assert_eq!(
            got, want,
            "the old unseen retirements, the live reminder's note aside"
        );
    }

    async fn open_reminder(core: &Core, moment_id: &str, artifact_id: &str) {
        sqlx::query!(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, source, created_at)
             VALUES (?, ?, 'due', ?, 'UTC', 'set', ?)",
            moment_id,
            artifact_id,
            crate::store::now() + 3_600,
            crate::store::now()
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_fresh_merge_keeps_its_sources_out_of_the_reap() {
        let core = test_core().await;
        let ids = seed(&core, &["side one of the fact", "side two of the fact"]).await;
        let merge = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "the fact, once".into(),
                    title: Some("M".into()),
                    ..Default::default()
                },
                &ids,
            )
            .await
            .unwrap();
        for id in &ids {
            core.store
                .set_superseded_by(id, Some(&merge.id))
                .await
                .unwrap();
            backdate_retired_at(&core, id, 100 * 86_400).await;
        }
        let (cands, _) = nominees(&core).await.unwrap();
        assert!(cands.is_empty(), "a fresh merge's sources are protected");

        sqlx::query!(
            "UPDATE artifacts SET created_at = ? WHERE id = ?",
            crate::store::now() - 100 * 86_400,
            &merge.id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(cands.len(), 2, "an old merge has forfeited its undo window");
    }

    #[tokio::test]
    async fn the_retired_note_its_replacement_and_live_notes_are_what_is_shown() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, 0.5);
        let ids = seed(
            &core,
            &[
                "the old wording",
                "the new wording",
                "a live note beside it",
            ],
        )
        .await;
        for id in &ids {
            crate::jobs::embed::run(&core, id).await.unwrap();
        }
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        backdate_retired_at(&core, &ids[0], 100 * 86_400).await;

        run(&core).await.unwrap();

        assert_eq!(dec.calls(), 1);
        let (state, questions) = dec.requests().remove(0);
        assert_eq!(questions[0].key, "reap_worth");
        for want in [
            "Retired note",
            "the old wording",
            "Its replacement",
            "the new wording",
            "Live notes",
            "a live note beside it",
        ] {
            assert!(state.contains(want), "{want} is missing: {state}");
        }
    }

    #[tokio::test]
    async fn a_disabled_sweep_does_not_run_a_unit_that_was_already_pending() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        core.reap.enabled = false;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (0, 0, 0));
        assert_eq!(dec.calls(), 0);
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "stale duplicate fact",
            "the one stage that destroys text must re-check its own gate"
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_spent_budget_buries_nothing_and_the_decision_waits() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        // Judging goes on past the cap; only the burial waits for the window to move.
        assert_eq!((report.judged, report.reaped), (1, 0));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
    }

    #[tokio::test]
    async fn a_burial_past_the_daily_cap_waits_and_happens_later_without_a_second_call() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, SURE_NO);
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let first = run(&core).await.unwrap();
        assert_eq!(first.reaped, 0);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        assert_eq!(dec.calls(), 1);
        // The next sweep does not ask again about a burial that is only waiting.
        let again = run(&core).await.unwrap();
        assert_eq!((again.judged, again.reaped), (0, 0));
        assert_eq!(dec.calls(), 1);

        core.evolve.max_actions_per_day = 20;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(dec.calls(), 1, "the waiting burial was asked about again");
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        let d = decision(&core, &ids[0], None).await;
        assert_eq!(d.state, State::Done);
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_burial_taken_back_before_is_not_bought_again() {
        use crate::store::actions::{Kind, UndoneBy};
        let mut core = test_core().await;
        let dec = deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        core.store.record_action(&row(&ids[0])).await.unwrap();
        core.store
            .undo_action_on(&ids[0], Kind::Reap, UndoneBy::Operator, "restored")
            .await
            .unwrap();

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (0, 0));
        assert_eq!(
            dec.calls(),
            0,
            "the judge was asked about a burial taken back"
        );
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .reaped_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_sure_no_buries_and_the_burial_is_the_decisions_action() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 1, 0));

        let row = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(row.text, "");
        assert!(row.reaped_at.is_some());
        let (text, meta, _) = core
            .store
            .graveyard_row(&ids[0])
            .await
            .unwrap()
            .expect("a grave");
        assert!(text.contains("stale duplicate fact"));
        let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert!(meta.get("reason").is_none(), "{meta}");
        let rows = core
            .store
            .open_actions(&[crate::store::actions::Kind::Reap], 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_id, ids[0]);
        assert_eq!(rows[0].detail, None);
        let d = decision(&core, &ids[0], None).await;
        assert_eq!(
            (d.band, d.state, d.pick.as_str()),
            (Band::Act, State::Done, "no")
        );
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap(),
            vec![rows[0].id.clone()]
        );
        let kept: i64 = sqlx::query_scalar!(
            "SELECT vec IS NOT NULL FROM graveyard WHERE id = ?",
            &ids[0]
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(kept, 1, "the vector was not kept in the grave");
        assert!(
            core.vectors
                .payloads_of(std::slice::from_ref(&ids[0]))
                .await
                .unwrap()
                .is_empty(),
            "the point must be gone"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker must be cleared once the delete is acknowledged"
        );
    }

    #[tokio::test]
    async fn restoring_that_note_marks_the_decision_undone() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        assert_eq!(run(&core).await.unwrap().reaped, 1);

        crate::web::ops::reactivate_artifact(&tenant(&core), &ids[0])
            .await
            .unwrap();

        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "stale duplicate fact"
        );
        assert_eq!(
            decision(&core, &ids[0], None).await.outcome,
            Some(Outcome::Undone)
        );
    }

    #[tokio::test]
    async fn a_sure_yes_arms_a_rescue_and_buries_nothing() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_YES);
        let ids = seed(&core, &["the admin port is 8443"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 0, 1));
        assert!(core.store.live_job(Stage::Rescue, &ids[0]).await.unwrap());
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        let old = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(old.text, "the admin port is 8443");
        assert!(
            old.superseded_by.is_none(),
            "the sweep writes nothing itself"
        );
        let d = decision(&core, &ids[0], None).await;
        assert_eq!(
            (d.band, d.state, d.pick.as_str()),
            (Band::Act, State::Done, "yes")
        );
    }

    #[tokio::test]
    async fn in_between_is_an_unsure_row_and_the_note_stays() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, 0.3);
        let ids = seed(&core, &["half a fact"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (1, 0, 0));
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.band, d.state), (Band::Ask, State::Open));
        let buttons = ReapApplier.buttons(&core, &d).await.unwrap();
        assert_eq!(
            buttons,
            vec![
                ("yes".to_string(), "Valuable".to_string()),
                ("no".to_string(), "Worthless".to_string())
            ]
        );
        assert_eq!(ReapApplier.label(), "Worth keeping?");
        let subjects = ReapApplier.subjects(&core, &d).await.unwrap();
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].0, ids[0]);
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "half a fact"
        );
        assert!(!core.store.live_job(Stage::Rescue, &ids[0]).await.unwrap());

        // Asked once: an open question is not asked again by the next sweep.
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 1);
        assert_eq!(core.store.open_ask_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_burial_line_is_the_higher_one() {
        let mut core = test_core().await;
        core.decide_cfg.bands.per.insert(
            "reap_worth".into(),
            crate::config::BandOverride {
                act_at: Some(0.97),
                ask_at: None,
            },
        );
        deciding(&mut core, 0.05);
        let ids = seed(&core, &["a fact the live base mostly has"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        assert_eq!(report.reaped, 0);
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.pick.as_str(), d.band), ("no", Band::Ask));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_person_pressing_worthless_buries_past_the_cap() {
        let mut core = test_core().await;
        deciding(&mut core, 0.7);
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        let ids = seed(&core, &["a fact a person reads"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        assert_eq!(d.state, State::Open);

        let d = crate::core::judging::answer(&core, &d.id, Some("no"))
            .await
            .unwrap();

        assert_eq!(d.outcome, Some(Outcome::Corrected));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn reap_runs_without_a_synthesize_role_and_only_rescue_needs_one() {
        let mut core = crate::core::test_support::test_core_without_synthesizer().await;
        let d = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|state, qs| {
            answers(
                qs,
                if state.contains("bury me") {
                    SURE_NO
                } else {
                    SURE_YES
                },
            )
        }));
        core.decider = d;
        let ids = seed(&core, &["bury me", "keep me"]).await;
        for id in &ids {
            deprecate_long_ago(&core, id).await;
        }

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped, report.rescued), (2, 1, 0));
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        assert!(
            !core.store.live_job(Stage::Rescue, &ids[1]).await.unwrap(),
            "nothing can write a rescue, so none is armed"
        );
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().text,
            "keep me"
        );
    }

    #[tokio::test]
    async fn restoring_a_buried_superseded_artifact_brings_its_text_back() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["the loser's own words", "the keeper"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        core.supersede(&ids[0], &ids[1]).await.unwrap();
        backdate_retired_at(&core, &ids[0], 400 * 86_400).await;
        let report = run(&core).await.unwrap();
        assert_eq!(report.reaped, 1);
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .text
                .is_empty()
        );

        core.unsupersede(&ids[0]).await.unwrap();

        let row = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(row.text, "the loser's own words", "not an empty row");
        assert!(row.reaped_at.is_none());
        assert!(row.superseded_by.is_none());
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        assert_eq!(
            row.embed_state,
            crate::store::artifacts::EmbedState::Pending,
            "and a fresh point is queued for it"
        );
    }

    #[tokio::test]
    async fn a_failed_reply_leaves_the_candidate_untouched() {
        let mut core = test_core().await;
        failing(&mut core, || crate::error::Error::Inference {
            role: "decide",
            detail: "no reply".into(),
        });
        let ids = seed(&core, &["still here"]).await;
        deprecate_long_ago(&core, &ids[0]).await;

        let report = run(&core).await.unwrap();
        // `did_work` reads any non-zero count as work; failed calls must leave the backoff free to
        // engage.
        assert_eq!((report.judged, report.reaped), (0, 0));
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "still here"
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_candidate_the_judge_refuses_stands_down_and_lets_the_queue_move() {
        let mut core = test_core().await;
        failing(&mut core, || crate::error::Error::InferenceRejected {
            role: "decide",
            detail: "HTTP 400: input too long".into(),
        });
        let ids = seed(&core, &["the one it will not read"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        let before = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (0, 0));
        let after = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();
        assert!(
            after > before,
            "a refusal is a verdict about this row: it goes to the back, {before} → {after}"
        );
    }

    #[tokio::test]
    async fn a_sweep_whose_decider_is_down_leaves_every_candidate_where_it_was() {
        let mut core = test_core().await;
        failing(&mut core, || crate::error::Error::InferenceBusy {
            role: "decide",
            detail: "HTTP 503".into(),
        });
        let ids = seed(&core, &["waiting for a judge"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        let before = core
            .store
            .get_artifact(&ids[0])
            .await
            .unwrap()
            .retired_at
            .unwrap();

        run(&core).await.unwrap();
        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .retired_at
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_buried_stub_is_out_of_reach_by_consequence_not_by_filter() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["a fact about zeppelins", "its living successor"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        backdate_retired_at(&core, &ids[0], 100 * 86_400).await;
        run(&core).await.unwrap();

        let q = crate::core::search::SearchQuery {
            q: "zeppelins".into(),
            limit: 10,
            tags: vec![],
            category: None,
            mark: false,
            rerank: false,
            explain: false,
            include_deprecated: true,
            include_superseded: true,
        };
        let hits = core
            .search(&q, crate::store::feedback::Door::Ui)
            .await
            .unwrap();
        assert!(
            hits.iter().all(|h| h.artifact_id != ids[0]),
            "a reaped stub surfaced in search"
        );
        let stub = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(stub.superseded_by.as_deref(), Some(ids[1].as_str()));
    }

    #[tokio::test]
    async fn the_rescue_cap_holds() {
        let mut core = test_core().await;
        core.reap.max_rescues_per_run = 1;
        deciding(&mut core, SURE_YES);
        let ids = seed(&core, &["first fact", "second fact"]).await;
        for id in &ids {
            deprecate_long_ago(&core, id).await;
        }
        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.rescued), (2, 1));
    }

    #[tokio::test]
    async fn the_first_pass_stamps_the_unaged_and_nominates_none_of_them() {
        let core = test_core().await;
        let ids = seed(&core, &["pre-column retirement"]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = NULL WHERE id = ?",
            &ids[0]
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        let (cands, stamped) = nominees(&core).await.unwrap();
        assert_eq!(stamped, 1);
        assert!(cands.is_empty(), "a fresh clock has not run");
    }

    // ---- asked once per era; a person's "valuable" stands ----

    /// A decider under another model name: a new era for every question.
    struct NextEra(std::sync::Arc<crate::infer::fake::RuleDecider>);

    #[async_trait::async_trait]
    impl crate::infer::decide::Decider for NextEra {
        async fn decide(
            &self,
            state: &str,
            qs: &[crate::infer::decide::Question],
        ) -> Result<crate::infer::decide::Answers> {
            self.0.decide(state, qs).await
        }
        fn context_tokens(&self) -> usize {
            self.0.context_tokens()
        }
        fn model(&self) -> &str {
            "rule-decider-next"
        }
    }

    async fn bump_rev(core: &Core, id: &str) {
        sqlx::query!(
            "UPDATE artifacts SET embed_rev = embed_rev + 1 WHERE id = ?",
            id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_note_the_model_left_is_not_nominated_again_until_it_changes() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, 0.5);
        let ids = seed(&core, &["a torn fact"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.band, d.state), (Band::Leave, State::Done));
        let first = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;

        for _ in 0..3 {
            let (cands, _) = nominees(&core).await.unwrap();
            assert!(cands.is_empty(), "a note left in this era was nominated");
            run(&core).await.unwrap();
        }
        assert_eq!(dec.calls(), 1);
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().retired_at,
            first,
            "a note left is not stood down to be asked again"
        );

        bump_rev(&core, &ids[0]).await;
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 2, "a note that changed is asked again");
    }

    #[tokio::test]
    async fn a_lapsed_row_is_not_asked_again_in_the_era() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, 0.3);
        let ids = seed(&core, &["nobody answered this"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        assert_eq!(d.state, State::Open);
        // runtime: test-only backdating.
        sqlx::query("UPDATE decisions SET decided_at = decided_at - ? WHERE id = ?")
            .bind(31 * 86_400)
            .bind(&d.id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(crate::jobs::decisions::run(&core).await.unwrap().lapsed, 1);

        for _ in 0..2 {
            // However much time passes: the question ages past the note's retirement stamp.
            // runtime: test-only backdating.
            sqlx::query("UPDATE decisions SET decided_at = decided_at - ? WHERE id = ?")
                .bind(400 * 86_400)
                .bind(&d.id)
                .execute(&core.store.pool)
                .await
                .unwrap();
            backdate_retired_at(&core, &ids[0], 100 * 86_400).await;
            run(&core).await.unwrap();
        }
        assert_eq!(dec.calls(), 1);
    }

    #[tokio::test]
    async fn questions_waiting_at_the_head_do_not_take_the_next_notes_slots() {
        let mut core = test_core().await;
        core.reap.max_judged_per_run = 2;
        core.decide_cfg.ask_open_max = 1;
        let dec = deciding(&mut core, 0.7);
        let ids = seed(&core, &["oldest", "older", "next in line"]).await;
        for (i, id) in ids.iter().enumerate() {
            core.store
                .set_artifact_status(id, ArtifactStatus::Deprecated)
                .await
                .unwrap();
            backdate_retired_at(&core, id, (300 - i as i64) * 86_400).await;
        }
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 2);
        assert_eq!(decision(&core, &ids[0], None).await.state, State::Open);
        assert_eq!(decision(&core, &ids[1], None).await.state, State::Waiting);

        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(
            cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![ids[2].as_str()]
        );
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 3);
        decision(&core, &ids[2], None).await;
    }

    #[tokio::test]
    async fn a_person_s_valuable_is_never_overruled_by_a_later_sure_no() {
        let mut core = test_core().await;
        core.generator = None;
        deciding(&mut core, 0.7);
        let ids = seed(&core, &["a person keeps this"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let asked = decision(&core, &ids[0], None).await;
        crate::core::judging::answer(&core, &asked.id, Some("yes"))
            .await
            .unwrap();

        // The same era: not put to the model again.
        let dec = deciding(&mut core, SURE_NO);
        backdate_retired_at(&core, &ids[0], 400 * 86_400).await;
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 0);

        // A new era: still not.
        let next = std::sync::Arc::new(crate::infer::fake::RuleDecider::new(|_, qs| {
            answers(qs, SURE_NO)
        }));
        core.decider = std::sync::Arc::new(NextEra(next.clone()));
        backdate_retired_at(&core, &ids[0], 400 * 86_400).await;
        run(&core).await.unwrap();
        assert_eq!(next.calls(), 0);

        // And a model's sure "no" about it, however it came, buries nothing.
        let id = core
            .store
            .log_decision(&crate::store::decision_log::NewLogged {
                question_id: "reap_worth".into(),
                question_version: 1,
                model: "rule-decider-next".into(),
                subject_kind: "artifact",
                subject_a: ids[0].clone(),
                subject_b: None,
                rev_a: Some(asked.rev_a.unwrap()),
                rev_b: None,
                answer: "{}".into(),
                top: 0.995,
                pick: "no".into(),
                swapped_agree: None,
                band: Band::Act,
                reason: None,
                state: State::Waiting,
            })
            .await
            .unwrap();
        let d = core.store.logged(&id).await.unwrap().unwrap();
        backdate_retired_at(&core, &ids[0], 400 * 86_400).await;
        let rows = ReapApplier
            .apply(&core, &d, "no", crate::store::pairs::DecidedBy::Model)
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().text,
            "a person keeps this"
        );
    }

    #[tokio::test]
    async fn ask_again_on_an_answered_row_puts_the_note_back() {
        let mut core = test_core().await;
        core.generator = None;
        let dec = deciding(&mut core, 0.7);
        let ids = seed(&core, &["asked again by hand"]).await;
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        crate::core::judging::answer(&core, &d.id, Some("yes"))
            .await
            .unwrap();
        let retired = core.store.get_artifact(&ids[0]).await.unwrap().retired_at;
        crate::core::judging::ask_again(&core, &d.id).await.unwrap();

        // In its own place in the queue: asking again does not restart the note's age.
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().retired_at,
            retired
        );
        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(
            cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![ids[0].as_str()]
        );
        run(&core).await.unwrap();
        assert_eq!(dec.calls(), 2);
    }

    #[tokio::test]
    async fn reopening_a_decision_about_a_buried_note_changes_nothing() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        assert_eq!(run(&core).await.unwrap().reaped, 1);
        let d = decision(&core, &ids[0], None).await;
        let before = core.store.get_artifact(&ids[0]).await.unwrap();

        ReapApplier.reopen(&core, &d).await.unwrap();

        let after = core.store.get_artifact(&ids[0]).await.unwrap();
        assert_eq!(
            (after.retired_at, after.reaped_at, after.text.as_str()),
            (before.retired_at, before.reaped_at, "")
        );
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        assert!(nominees(&core).await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn a_waiting_burial_is_held_while_the_reap_is_off_or_the_base_may_not_act_alone() {
        use crate::config::Autonomy;
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        core.evolve.max_actions_per_day = 0;
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.band, d.state), (Band::Act, State::Waiting));
        core.evolve.max_actions_per_day = 20;

        core.reap.enabled = false;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 0, "the switch did not hold a waiting burial");
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        core.reap.enabled = true;

        // Below full no daily cap holds anything, so only the switch can.
        for level in [Autonomy::Off, Autonomy::Ranking] {
            core.evolve.autonomous = level;
            let r = crate::jobs::decisions::run(&core).await.unwrap();
            assert_eq!(r.applied, 0, "{level:?}");
            assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
            assert_eq!(decision(&core, &ids[0], None).await.state, State::Waiting);
        }

        core.evolve.autonomous = Autonomy::Full;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        assert_eq!(decision(&core, &ids[0], None).await.state, State::Done);
    }

    #[tokio::test]
    async fn the_act_itself_holds_a_model_s_burial_while_the_reap_is_off_and_not_a_person_s() {
        let mut core = test_core().await;
        deciding(&mut core, 0.3);
        let ids = seed(&core, &["a fact a person reads"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        run(&core).await.unwrap();
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.pick.as_str(), d.state), ("no", State::Open));
        core.reap.enabled = false;

        let rows = ReapApplier
            .apply(&core, &d, "no", DecidedBy::Model)
            .await
            .unwrap();
        assert!(rows.is_empty());
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());

        crate::core::judging::answer(&core, &d.id, Some("no"))
            .await
            .unwrap();
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_rescue_held_by_the_pacing_waits_without_failing_and_is_armed_when_there_is_room() {
        let mut core = test_core().await;
        core.reap.max_rescues_per_run = 1;
        deciding(&mut core, SURE_YES);
        let ids = seed(&core, &["first fact", "second fact"]).await;
        for (i, id) in ids.iter().enumerate() {
            core.store
                .set_artifact_status(id, ArtifactStatus::Deprecated)
                .await
                .unwrap();
            backdate_retired_at(&core, id, (300 - i as i64) * 86_400).await;
        }
        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.rescued), (2, 1));
        assert!(core.store.live_job(Stage::Rescue, &ids[0]).await.unwrap());
        let held = decision(&core, &ids[1], None).await;
        assert_eq!((held.band, held.state), (Band::Act, State::Waiting));

        // A wait, not a failed apply: no error comes back and the decision is as it was.
        let again = crate::core::judging::settle(&core, held.clone())
            .await
            .unwrap();
        assert_eq!(again.state, State::Waiting);
        crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(decision(&core, &ids[1], None).await.state, State::Waiting);
        assert!(!core.store.live_job(Stage::Rescue, &ids[1]).await.unwrap());

        core.reap.max_rescues_per_run = 2;
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!(r.applied, 1);
        assert_eq!(decision(&core, &ids[1], None).await.state, State::Done);
        assert!(core.store.live_job(Stage::Rescue, &ids[1]).await.unwrap());
    }

    #[tokio::test]
    async fn more_questions_than_a_page_at_the_head_still_let_the_next_note_through() {
        let core = test_core().await;
        // One more than `nominees` reads in a page at the configured cap.
        let page = core.reap.max_judged_per_run as i64 * OVERFETCH;
        let texts: Vec<String> = (0..=page).map(|i| format!("asked about {i}")).collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        for id in &seed(&core, &texts).await {
            core.store
                .set_artifact_status(id, ArtifactStatus::Deprecated)
                .await
                .unwrap();
            backdate_retired_at(&core, id, 300 * 86_400).await;
        }
        let asked = catalogue::reap_worth();
        let head = core
            .store
            .reap_candidates(min_age_secs(&core), &reap_era(&core), i64::MAX)
            .await
            .unwrap();
        assert_eq!(head.len() as i64, page + 1);
        // Each with its question waiting for a seat, as a run leaves it.
        for c in &head {
            core.store
                .log_decision(&crate::store::decision_log::NewLogged {
                    question_id: asked.id.into(),
                    question_version: asked.version,
                    model: core.decider.model().into(),
                    subject_kind: "artifact",
                    subject_a: c.id.clone(),
                    subject_b: None,
                    rev_a: Some(c.embed_rev),
                    rev_b: None,
                    answer: "{}".into(),
                    top: 0.7,
                    pick: "yes".into(),
                    swapped_agree: None,
                    band: Band::Ask,
                    reason: None,
                    state: State::Waiting,
                })
                .await
                .unwrap();
        }
        let next = seed(&core, &["next in line"]).await;
        core.store
            .set_artifact_status(&next[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&core, &next[0], 200 * 86_400).await;

        let (cands, _) = nominees(&core).await.unwrap();
        assert_eq!(
            cands.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![next[0].as_str()],
            "the questions at the head filled the page"
        );
    }

    use crate::vector::{
        Facets, LifecycleRow, SearchFilter, SearchHit, StoredLifecycle, Touch, VectorPayload,
        VectorPoint, VectorStore, sparse::SparseVector,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    /// The store beneath, except where a test says it fails.
    struct Faulty {
        inner: Arc<dyn VectorStore>,
        no_delete: bool,
        /// The note whose neighbours cannot be read.
        no_neighbours_of: Option<String>,
    }

    fn down() -> Error {
        Error::Vector("connection refused".into())
    }

    #[async_trait::async_trait]
    impl VectorStore for Faulty {
        async fn ensure_collection(&self, dim: usize) -> Result<()> {
            self.inner.ensure_collection(dim).await
        }
        async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
            self.inner.upsert(points).await
        }
        async fn set_payload(&self, payload: &VectorPayload) -> Result<()> {
            self.inner.set_payload(payload).await
        }
        async fn set_lifecycle(
            &self,
            artifact_id: &str,
            status: ArtifactStatus,
            superseded_by: Option<&str>,
        ) -> Result<()> {
            self.inner
                .set_lifecycle(artifact_id, status, superseded_by)
                .await
        }
        async fn set_last_verified_at(
            &self,
            artifact_id: &str,
            at: i64,
            reset_hits: bool,
        ) -> Result<()> {
            self.inner
                .set_last_verified_at(artifact_id, at, reset_hits)
                .await
        }
        async fn stale_candidates(
            &self,
            older_than: i64,
            max_hits: i64,
            limit: usize,
        ) -> Result<Vec<SearchHit>> {
            self.inner
                .stale_candidates(older_than, max_hits, limit)
                .await
        }
        async fn search(
            &self,
            vector: &[f32],
            sparse: &SparseVector,
            limit: usize,
            filter: &SearchFilter,
        ) -> Result<Vec<SearchHit>> {
            self.inner.search(vector, sparse, limit, filter).await
        }
        async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
            self.inner.touch(targets, seen_at).await
        }
        async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()> {
            self.inner.apply_lifecycle(rows).await
        }
        async fn lifecycle_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, StoredLifecycle>> {
            self.inner.lifecycle_of(artifact_ids).await
        }
        async fn all_artifact_ids(&self) -> Result<Vec<String>> {
            self.inner.all_artifact_ids().await
        }
        async fn payloads_of(
            &self,
            artifact_ids: &[String],
        ) -> Result<HashMap<String, VectorPayload>> {
            self.inner.payloads_of(artifact_ids).await
        }
        async fn resurface(
            &self,
            limit: usize,
            older_than: i64,
            unseen_since: i64,
        ) -> Result<Vec<SearchHit>> {
            self.inner
                .resurface(limit, older_than, unseen_since)
                .await
        }
        async fn facets(&self, limit: usize) -> Result<Facets> {
            self.inner.facets(limit).await
        }
        async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
            if self.no_neighbours_of.as_deref() == Some(artifact_id) {
                return Err(down());
            }
            self.inner.neighbours(artifact_id, limit).await
        }
        async fn set_context_vectors(
            &self,
            artifact_id: &str,
            vectors: Vec<Vec<f32>>,
        ) -> Result<()> {
            self.inner.set_context_vectors(artifact_id, vectors).await
        }
        async fn context_query(
            &self,
            vector: &[f32],
            limit: usize,
            filter: &SearchFilter,
        ) -> Result<Vec<SearchHit>> {
            self.inner.context_query(vector, limit, filter).await
        }
        async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
            if self.no_delete {
                return Err(down());
            }
            self.inner.delete_artifacts(artifact_ids).await
        }
        async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
            self.inner.delete_by_corpus(corpus_id).await
        }
        async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
            self.inner.sample(limit).await
        }
        async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
            self.inner.dense_of(artifact_id).await
        }
        async fn count(&self) -> Result<u64> {
            self.inner.count().await
        }
    }

    #[tokio::test]
    async fn a_burial_whose_point_delete_fails_stands_and_the_repair_pass_finishes_it() {
        let mut core = test_core().await;
        deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["stale duplicate fact"]).await;
        crate::jobs::embed::run(&core, &ids[0]).await.unwrap();
        deprecate_long_ago(&core, &ids[0]).await;
        let inner = core.vectors.clone();
        core.vectors = Arc::new(Faulty {
            inner: inner.clone(),
            no_delete: true,
            no_neighbours_of: None,
        });

        let report = run(&core).await.unwrap();
        assert_eq!(report.reaped, 1);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_some());
        let d = decision(&core, &ids[0], None).await;
        assert_eq!((d.state, d.reason.as_deref()), (State::Done, None));
        assert_eq!(
            core.store.decision_action_ids(&d.id).await.unwrap().len(),
            1
        );
        // The next sweep finds a decision that acted, not a subject that changed.
        let r = crate::jobs::decisions::run(&core).await.unwrap();
        assert_eq!((r.applied, r.reasked), (0, 0));
        assert_eq!(decision(&core, &ids[0], None).await.reason, None);
        assert_eq!(
            core.store.dirty_lifecycle_artifacts(10).await.unwrap().len(),
            1,
            "the marker must stay for the repair pass"
        );
        assert!(!inner.payloads_of(&ids).await.unwrap().is_empty());

        core.vectors = inner;
        assert_eq!(
            crate::jobs::consolidate::repair_lifecycle_drift(&core)
                .await
                .unwrap(),
            1
        );
        assert!(core.vectors.payloads_of(&ids).await.unwrap().is_empty());
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_note_whose_neighbours_cannot_be_read_does_not_stop_the_sweep() {
        let mut core = test_core().await;
        let dec = deciding(&mut core, SURE_NO);
        let ids = seed(&core, &["its neighbours cannot be read", "behind it"]).await;
        for (i, id) in ids.iter().enumerate() {
            crate::jobs::embed::run(&core, id).await.unwrap();
            core.store
                .set_artifact_status(id, ArtifactStatus::Deprecated)
                .await
                .unwrap();
            backdate_retired_at(&core, id, (300 - i as i64) * 86_400).await;
        }
        core.vectors = Arc::new(Faulty {
            inner: core.vectors.clone(),
            no_delete: false,
            no_neighbours_of: Some(ids[0].clone()),
        });

        let report = run(&core).await.unwrap();
        assert_eq!((report.judged, report.reaped), (1, 1));
        assert_eq!(dec.calls(), 1);
        assert!(core.store.graveyard_row(&ids[0]).await.unwrap().is_none());
        assert!(core.store.graveyard_row(&ids[1]).await.unwrap().is_some());
    }
}
