pub mod associate;
pub mod consolidate;
pub mod context;
pub mod dedupe;
pub mod describe;
pub mod embed;
pub mod extract;
pub mod gaps;
pub mod judgement;
pub mod merge;
pub mod passages;
pub mod promote;
pub mod pursuit;
pub mod reap;
pub mod reconcile;
pub mod relate;
pub mod remind;
pub mod retention;
pub mod synthesize;
pub mod window;

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::jobs::{Job, MAX_ATTEMPTS, Stage};
use std::time::Duration;
use tracing::Instrument;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A job still marked `running` after this long belonged to a process that died.
pub const STUCK_AFTER_SECS: i64 = 600;

/// Supersede `loser` by `winner`, and carry on if it cannot be done.
///
/// `supersede` refuses a side that is no longer active, and every caller here
/// read those statuses a moment ago — so an operator deprecating one in
/// between is an ordinary race, not a reason to fail the caller's whole
/// sweep or unit. Returns whether it happened.
pub(crate) async fn try_supersede(core: &Core, loser: &str, winner: &str, why: &str) -> bool {
    match core.supersede(loser, winner).await {
        Ok(()) => {
            tracing::info!(superseded = %loser, by = %winner, why);
            true
        }
        Err(e) => {
            tracing::warn!(superseded = %loser, by = %winner, error = %e, "could not hide {why}; it stays active");
            false
        }
    }
}

/// Claim one unit and run it, whoever it belongs to.
///
/// What a worker calls. The queue is instance-wide; the work is not. The
/// subject comes off the claimed row and names the core the unit runs against,
/// so a worker never holds a tenant across two units — which is what makes
/// this round-robin between users without a scheduler in it.
pub async fn run_any(tenants: &crate::tenants::Tenants) -> Result<bool> {
    let Some((subject, job)) = tenants.control().claim_job().await? else {
        return Ok(false);
    };
    let core = match tenants.get(&subject).await {
        Ok(t) => t.core,
        // The user was deleted between the enqueue and the claim. Their queue
        // goes with the row cascade, but a unit already in a worker's hand can
        // outlive it, and retrying it can never succeed.
        Err(Error::NotFound) => {
            tracing::info!(subject = %subject, "queue row for a user that no longer exists; dropping");
            tenants.control().complete_job(job.id).await?;
            return Ok(true);
        }
        // Anything else is a fault of this moment rather than of the row: a
        // Qdrant that will not answer, a file that will not open. The claim
        // above already happened, so returning here would leave the unit
        // `running` with nobody holding it — one attempt spent, no
        // `last_error`, and nothing until the hourly `reclaim_stuck` notices.
        // Put it back on the queue at the ordinary backoff, which is what
        // every other failure in this module does, and then let the worker
        // loop log and pause on the error rather than spinning through the
        // whole queue one claim at a time while the endpoint is down.
        Err(e) => {
            tracing::warn!(
                subject = %subject,
                error = %e,
                "could not open the base this unit belongs to; requeueing it"
            );
            tenants
                .control()
                .fail_job(job.id, job.attempts, &e.to_string())
                .await?;
            return Err(e);
        }
    };
    run_dispatched(&core, job, Some(&subject)).await
}

/// Claim and run at most one of *this tenant's* jobs. Returns false when their
/// queue is empty, which is the loop's signal to sleep.
///
/// Beside `run_any` rather than replaced by it: "take this base's next step" is
/// a real operation, and every test that drives a capture to completion is
/// asking for exactly it.
pub async fn run_one(core: &Core) -> Result<bool> {
    let Some(job) = core.store.claim_job().await? else {
        return Ok(false);
    };
    run_dispatched(core, job, None).await
}

async fn run_dispatched(core: &Core, job: Job, subject: Option<&str>) -> Result<bool> {
    let span = tracing::info_span!(
        "job",
        id = job.id,
        subject = subject.unwrap_or(""),
        stage = job.stage.as_str(),
        target = %job.target_id,
        attempt = job.attempts
    );
    // `.instrument`, not `span.enter()`: a guard held across an `.await` does
    // not travel with the future, so every line logged after the first
    // suspension point lost its `job{...}` prefix — which in a journal full of
    // interleaved windows is the only thing saying which job spoke.
    run_claimed(core, job).instrument(span).await
}

async fn run_claimed(core: &Core, job: Job) -> Result<bool> {
    // Only a sweep answers this, and only a sweep is asked: `did_work`
    // governs how long until the next *periodic* run, and a unit that is not
    // periodic has none.
    let mut did_work = false;
    let result = match (job.stage, job.target_kind.as_str()) {
        (Stage::Synthesize | Stage::Enrich, _) => synthesize::plan(core, &job.target_id).await,
        // Embedding is batched per source; the per-chunk path is for edits,
        // for oversize splits, and for isolating a chunk the batch chokes on.
        (Stage::Embed, "corpus") => embed::run_corpus(core, &job.target_id).await,
        (Stage::Embed, _) => embed::run(core, &job.target_id).await,
        (Stage::LinkJudge, _) => associate::judge(core, &job.target_id).await,
        (Stage::SegmentWindow, _) => window::run(core, &job.target_id).await,
        (Stage::Title, _) => synthesize::run_title(core, &job.target_id).await,
        (Stage::Dedupe, _) => dedupe::run(core, &job.target_id).await,
        (Stage::Relate, _) => relate::run(core, &job.target_id).await,
        (Stage::Describe, _) => describe::run(core, &job.target_id).await,
        (Stage::Extract, _) => extract::run(core, &job.target_id).await,
        (Stage::Generate, _) => pursuit::generate(core, &job.target_id).await,
        // Every periodic unit goes through one path, because every periodic
        // unit is accounted for. The sweeps below look at the whole collection,
        // so they ignore the target.
        (
            Stage::Consolidate
            | Stage::Associate
            | Stage::Pursuit
            | Stage::Retention
            | Stage::ArmDedupe
            | Stage::Context
            | Stage::Reap,
            _,
        ) => run_accounted(core, job.stage).await.map(|w| did_work = w),
        (Stage::Remind, _) => remind::run(core).await,
    };

    match result {
        Ok(()) => {
            core.store.complete_job(job.id).await?;
            // After completing, never before: the queue is keyed by (stage,
            // target), so a handler that re-armed itself would be upserting the
            // very row this `complete_job` then closes.
            if job.stage == Stage::Embed && job.target_kind == "corpus" {
                embed::rearm_if_more(core, &job.target_id).await?;
            }
            // Same rule, and the same reason `remind::run` does not do this
            // itself: the unit sleeps until the *earliest* owed moment, so
            // every run has to name the next one. `arm_at` refuses a `running`
            // row — that guard is what stops a queued sweep receding forever —
            // so an arming from inside the handler was silently a no-op, and
            // with two reminders due the second never fired until an unrelated
            // write happened to re-arm the unit.
            // Logged and carried past, the way `rearm_periodic` treats its own
            // failure. `complete_job` has already committed, so propagating
            // recovers nothing here and only turns a lost arming into a lost
            // arming plus a failed run. `arm_missing_periodic` is what actually
            // puts the unit back — see the tail of it, where `Remind` is armed
            // for exactly this reason — and a lost arming now costs one repair
            // interval rather than lasting until an unrelated write.
            if job.stage == Stage::Remind
                && let Err(e) = core.store.rearm_remind().await
            {
                tracing::warn!(error = %e, "could not re-arm the reminder unit; the repair pass picks it up");
            }
            rearm_periodic(core, &job, did_work).await;
            arm_successor(core, &job).await;
            Ok(true)
        }
        // The target was deleted while the job waited. Retrying can never
        // succeed, so close the job instead of burning attempts.
        Err(Error::NotFound) => {
            tracing::info!("target no longer exists; dropping job");
            core.store.complete_job(job.id).await?;
            Ok(true)
        }
        Err(e) if e.retryable() => {
            let exhausted = job.attempts >= MAX_ATTEMPTS;
            match (job.stage, job.target_kind.as_str()) {
                // The two units that stop asking. A name is decoration and the
                // corpus already has its fallback. A pair the model will not
                // judge stays pending, and `pairs_to_judge` orders it behind
                // the ones that have been asked less, so a later sweep decides
                // again whether it is worth asking about. Everything else
                // stays queued at the backoff ceiling, because everything else
                // carries knowledge that would otherwise be lost.
                (Stage::Dedupe | Stage::Title, _) if exhausted => {
                    tracing::warn!(error = %e, stage = job.stage.as_str(), "giving up on this unit for now");
                    core.store.complete_job(job.id).await?;
                }
                // The photo is stored, so nothing is lost by stopping — but a
                // corpus shown as in flight forever is a lie. Unless the role
                // is simply not configured, which is a wait, not a failure.
                (Stage::Describe, _) if exhausted && core.describer.is_some() => {
                    tracing::warn!(error = %e, "could not read this image; parking it");
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // The PDF is stored, so nothing is lost by stopping — but a
                // corpus shown as in flight forever is a lie. No role guard,
                // unlike `Describe`: extraction needs nothing configured, so
                // this can never be a wait for a role that has not arrived.
                (Stage::Extract, _) if exhausted => {
                    tracing::warn!(error = %e, "could not extract this PDF; parking it");
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // A whole source failing together usually means the endpoint is
                // down, but it can also be one chunk the embedder rejects.
                // Retrying chunk by chunk isolates the culprit either way.
                (Stage::Embed, "corpus") if exhausted => {
                    tracing::warn!(error = %e, "batch embedding exhausted retries; retrying chunk by chunk");
                    split_or_fail(core, &job, &e).await?;
                }
                _ => {
                    tracing::warn!(error = %e, "job failed; will retry");
                    if job.stage == Stage::Embed && exhausted {
                        settle_failed_artifact(core, &job, &e).await?;
                    } else {
                        core.store
                            .fail_job(job.id, job.attempts, &e.to_string())
                            .await?;
                    }
                }
            }
            Ok(true)
        }
        Err(e) => {
            tracing::error!(error = %e, "job failed permanently");
            match (job.stage, job.target_kind.as_str()) {
                // Refused as a batch does not mean refused as chunks; the same
                // isolation the exhausted path buys is worth buying here.
                (Stage::Embed, "corpus") => split_or_fail(core, &job, &e).await?,
                (Stage::Embed, _) => settle_failed_artifact(core, &job, &e).await?,
                (Stage::Describe | Stage::Extract, _) => {
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // Kept armed at the unit's own attempt count, floored at
                // `MAX_ATTEMPTS`: the first refusal waits out the ordinary
                // budget, and later ones keep backing off rather than pinning
                // the delay at one value for ever.
                _ => {
                    core.store
                        .fail_job(job.id, job.attempts.max(MAX_ATTEMPTS), &e.to_string())
                        .await?;
                }
            }
            Ok(true)
        }
    }
}

/// Run one periodic unit and write down what it did.
///
/// The counts are the ones each sweep already returns; nothing here is counted
/// for the account's sake. A failed run is recorded too — it is exactly the run
/// an operator needs to see, and a history that kept only the successes would
/// show a sweep going quiet with nothing anywhere saying why.
///
/// The row is written whatever happens to the unit afterwards, which is why it
/// is written here rather than beside `complete_job`: a failed sweep stays
/// queued behind a backoff, and that is a retry of the same unit rather than a
/// run that never happened.
async fn run_accounted(core: &Core, stage: Stage) -> Result<bool> {
    let started_at = crate::store::now();
    let outcome = match stage {
        Stage::Consolidate => consolidate::run(core).await.and_then(detail),
        Stage::Associate => associate::run(core).await.and_then(detail),
        Stage::Retention => retention::run(core).await.and_then(detail),
        Stage::Reap => reap::run(core).await.and_then(detail),
        Stage::Context => context::run(core).await.and_then(detail),
        Stage::Pursuit => pursuit::run(core)
            .await
            .and_then(|n| detail(serde_json::json!({ "pursuits": n }))),
        Stage::ArmDedupe => consolidate::arm_dedupe(core).await.and_then(|n| {
            if n > 0 {
                tracing::info!(armed = n, "armed dedupe units");
            }
            detail(serde_json::json!({ "armed": n }))
        }),
        // `run_claimed` sends only the sweeps above here, and a sixth arriving
        // silently unaccounted for is worse than a row saying so.
        _ => detail(serde_json::json!({})),
    };
    let (state, written) = match &outcome {
        Ok(d) => ("ok", d.clone()),
        Err(e) => (
            "failed",
            serde_json::json!({ "error": e.to_string() }).to_string(),
        ),
    };
    if let Err(e) = core
        .store
        .record_sweep_run(stage.as_str(), started_at, state, &written)
        .await
    {
        tracing::warn!(stage = stage.as_str(), error = %e, "could not record what the sweep did");
    }
    outcome.map(|d| did_work(&d))
}

/// Whether a sweep's own account says it did anything.
///
/// Read off the counts it already writes into `sweep_runs.detail` rather than
/// each sweep learning to answer separately: every report is a flat object of
/// numbers, so any non-zero one is work, and a report that gains a field keeps
/// working without being told about this.
///
/// Only the *flat* numbers. A count nested one level down is read by nobody
/// here, and that is the escape hatch a report uses for a standing count — a
/// number saying what exists rather than what this pass did. Left flat, such a
/// count claims work on every run over unchanged data and the backoff never
/// engages at all, which is how the retention and context sweeps each woke a
/// dormant base every interval to report the clusters it already had. See
/// `retention::Standing` and `context::Standing`.
///
/// An unreadable report is not work. Claiming otherwise would reset the backoff
/// on a base where nothing is happening, which is the one case the backoff is
/// for.
fn did_work(detail: &str) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(detail)
    else {
        return false;
    };
    map.values()
        .filter_map(serde_json::Value::as_f64)
        .any(|n| n != 0.0)
}

/// The counts a sweep returned, as the JSON the account stores.
fn detail<T: serde::Serialize>(report: T) -> Result<String> {
    Ok(serde_json::to_string(&report).unwrap_or_else(|_| "{}".into()))
}

/// A sweep that has finished arms itself one interval out.
///
/// After completing, never before: the queue is keyed by `(stage, target)`, so
/// a handler that re-armed itself would be upserting the very row
/// `complete_job` then closes.
///
/// Nothing is done for a *failed* sweep, and nothing needs to be: `fail_job`
/// leaves the row pending behind a backoff, which is already the re-arming. One
/// failure ending a sweep for the life of the process is the one way this
/// design could quietly stop the memory from learning, and there is a test
/// saying it does not.
///
/// A sweep switched off since it was armed re-arms as nothing: `periodic_period`
/// returns `None`, the row stays closed, and the repair pass will not put it
/// back either, because it is no longer in `periodic_units`.
async fn rearm_periodic(core: &Core, job: &Job, did_work: bool) {
    rearm_periodic_with(core, job.stage, &job.target_id, did_work).await;
}

/// How long until this sweep runs again.
///
/// The configured period when the run did something, doubled per consecutive
/// empty run when it did not, capped at `schedule.backoff_max_hours`. A quiet
/// base therefore stops waking every interval to find nothing — which is what a
/// dormant tenant costs, multiplied by however many of them there are.
///
/// The reset comes free, and it is what makes this a backoff rather than a
/// firing rule: `arm_now` already pulls a sleeping unit's `run_after` forward to
/// zero and already clears the count, and every producer already calls it. New
/// data cancels the wait without a single producer change. In a firing world a
/// lost token stalls a transition for ever and silently; here a missed signal
/// costs one longer interval.
async fn rearm_periodic_with(core: &Core, stage: Stage, target: &str, did_work: bool) {
    let Some(period) = crate::core::background::periodic_period(core, stage) else {
        return;
    };
    let empty = if did_work {
        0
    } else {
        core.store.empty_runs(stage, target).await.unwrap_or(0) + 1
    };
    let cap = core.schedule.backoff_max_hours.saturating_mul(3600);
    // `empty - 1` doublings: the first empty run waits the configured period,
    // and the shift is bounded well under `u64`'s width so a long-dormant base
    // cannot wrap it back to something short.
    let wait = period
        .as_secs()
        .saturating_mul(1u64 << empty.clamp(1, 32).saturating_sub(1))
        .clamp(period.as_secs(), cap.max(period.as_secs()));
    let at = crate::store::now() + wait as i64;
    if let Err(e) = core
        .store
        .arm_periodic_with_backoff(stage, "collection", target, at, empty)
        .await
    {
        tracing::warn!(stage = stage.as_str(), error = %e, "could not re-arm the sweep");
    }
}

/// Replay before pursue.
///
/// The one ordering worth expressing beyond what the tree already expresses by
/// arming: a sitting scored against the links this run folded in, not against
/// the last half-hour's. The pursuit sweep keeps its own period as a floor, so
/// this pulls it forward rather than being the only thing that runs it.
async fn arm_successor(core: &Core, job: &Job) {
    if job.stage != Stage::Associate || !core.learn.enabled {
        return;
    }
    if let Err(e) = core
        .store
        .arm_now(
            Stage::Pursuit,
            "collection",
            crate::core::background::ASSOCIATE_TARGET,
        )
        .await
    {
        tracing::warn!(error = %e, "could not bring the pursuit sweep forward");
    }
}

/// Park an image that could not be read, tolerating its corpus having been
/// deleted in the meantime.
///
/// A corpus deleted before the job ran is caught far above, by the `NotFound`
/// arm that simply closes the unit. This is the narrower race: deleted between
/// the failed read and this write. Letting that `NotFound` out of `run_claimed`
/// would leave the job `running` until `reclaim_stuck` picks it up ten minutes
/// later, only to lose the same race again.
async fn park_failed_if_still_there(
    core: &Core,
    stage: Stage,
    corpus_id: &str,
    e: &Error,
) -> Result<()> {
    let reason = e.to_string();
    let parked = match stage {
        Stage::Extract => extract::park_failed(core, corpus_id, &reason).await,
        _ => describe::park_failed(core, corpus_id, &reason).await,
    };
    match parked {
        Err(Error::NotFound) => {
            tracing::info!(corpus_id, "corpus went away before it could be parked");
            Ok(())
        }
        other => other,
    }
}

/// Hand a batch embed that will not go through as a batch to per-chunk units.
/// The batch unit closes only once the split is queued; if even that fails,
/// the batch stays armed so the work is not lost.
async fn split_or_fail(core: &Core, job: &Job, e: &Error) -> Result<()> {
    match embed::split_into_artifact_jobs(core, &job.target_id).await {
        Ok(()) => core.store.complete_job(job.id).await,
        Err(fe) => {
            tracing::warn!(error = %fe, original = %e, "could not split the batch; keeping it queued");
            core.store
                .fail_job(job.id, job.attempts, &fe.to_string())
                .await
        }
    }
}

/// One chunk the embedder will not take: mark it, and let the corpus settle to
/// `partial` if this was the last one outstanding.
async fn settle_failed_artifact(core: &Core, job: &Job, e: &Error) -> Result<()> {
    // Kept armed at the backoff ceiling like every failed unit: no terminal
    // state, see `fail_job`.
    core.store
        .fail_job(job.id, job.attempts.max(MAX_ATTEMPTS), &e.to_string())
        .await?;
    core.store.mark_embed_failed(&job.target_id).await?;
    // A merged artifact belongs to no corpus, so there is no document whose
    // coverage its failure completes.
    if let Ok(c) = core.store.get_artifact(&job.target_id).await
        && let Some(corpus_id) = c.corpus_id.as_deref()
    {
        embed::settle_corpus(core, corpus_id).await?;
    }
    Ok(())
}

pub struct Worker;

impl Worker {
    /// The pool, over the whole instance.
    ///
    /// `server.workers` keeps meaning what it has always meant — how many
    /// things this machine does at once — however many people sign up. It is
    /// the admission point in front of one set of inference endpoints, and a
    /// pool per user would put that queueing in the model server's socket
    /// backlog instead: invisible, unordered, unfair.
    pub fn spawn(
        tenants: std::sync::Arc<crate::tenants::Tenants>,
        workers: usize,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        (0..workers.max(1))
            .map(|n| {
                let tenants = tenants.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    tracing::info!(worker = n, "worker started");
                    loop {
                        tokio::select! {
                            _ = shutdown.changed() => {
                                if *shutdown.borrow() { break; }
                            }
                            worked = run_any(&tenants) => {
                                match worked {
                                    Ok(true) => continue,
                                    Ok(false) => tokio::time::sleep(POLL_INTERVAL).await,
                                    Err(e) => {
                                        tracing::error!(error = %e, "worker loop error");
                                        tokio::time::sleep(POLL_INTERVAL).await;
                                    }
                                }
                            }
                        }
                    }
                    tracing::info!(worker = n, "worker stopped");
                })
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Run the queue dry. What every stage test does after a capture.
    pub async fn drain(core: &crate::core::Core) {
        while crate::jobs::run_one(core).await.unwrap() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::infer::fake::FakeEmbedder;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::{MAX_ATTEMPTS, backoff_secs};
    use std::sync::Arc;

    /// The row a sweep leaves behind, whatever state it left in.
    async fn pending_run_after(core: &Core, stage: Stage) -> Option<i64> {
        sqlx::query_scalar("SELECT run_after FROM jobs WHERE stage = ? AND state = 'pending'")
            .bind(stage.as_str())
            .fetch_optional(&core.store.control.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_sweep_that_ran_says_what_it_did() {
        // The question a system that describes itself as sleeping has to be
        // able to answer: what did the memory do while I was away.
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.store
            .arm_periodic(
                Stage::Associate,
                "collection",
                crate::core::background::ASSOCIATE_TARGET,
                0,
            )
            .await
            .unwrap();

        let job = core.store.claim_job().await.unwrap().unwrap();
        run_claimed(&core, job).await.unwrap();

        let runs = core.store.sweep_history(10).await.unwrap();
        assert_eq!(runs.len(), 1, "the run was never recorded");
        assert_eq!(runs[0].stage, "associate");
        assert_eq!(runs[0].outcome, "ok");
        assert!(
            runs[0].detail.contains("forgotten"),
            "the counts the sweep already returns were not kept: {}",
            runs[0].detail
        );
    }

    #[tokio::test]
    async fn a_finished_sweep_arms_itself_one_interval_out() {
        // No ticker holds the period any more: `run_after` is the cursor
        // recording when the sweep last ran, and it is already indexed.
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.store
            .arm_periodic(
                Stage::Associate,
                "collection",
                crate::core::background::ASSOCIATE_TARGET,
                0,
            )
            .await
            .unwrap();

        let job = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(job.stage, Stage::Associate);
        run_claimed(&core, job).await.unwrap();

        let at = pending_run_after(&core, Stage::Associate)
            .await
            .expect("the sweep did not arm itself again");
        let interval = core.associate.interval_mins as i64 * 60;
        let expected = crate::store::now() + interval;
        assert!(
            (at - expected).abs() <= 5,
            "armed at {at}, expected about {expected}"
        );
        // And exactly one of it: `UNIQUE(stage, target_id)` still does that
        // work, for free and for the same reason it always did.
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE stage = 'associate'")
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn a_sweep_that_failed_is_still_going_to_run_again() {
        // The one way this design could quietly stop the memory from learning:
        // one failure ending a sweep for the life of the process. `fail_job`
        // leaves the row pending behind a backoff, which is the re-arming — so
        // what this asserts is that nothing closes it instead.
        let mut core = test_core().await;
        core.learn.enabled = true;
        // A pursuit sweep with no store behind it fails; what it fails on does
        // not matter, only that the row survives it.
        core.store
            .arm_periodic(Stage::Pursuit, "collection", "collection", 0)
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        let id = job.id;
        core.store
            .fail_job(id, 1, "the endpoint was down")
            .await
            .unwrap();

        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(
            state, "pending",
            "a failed sweep was closed rather than left to retry"
        );
    }

    #[tokio::test]
    async fn a_sweep_switched_off_since_it_was_armed_does_not_arm_itself_again() {
        // The gates live on the list now, so this is what "switched off" means
        // for a unit already in the queue: it runs once more and stops.
        let mut core = test_core().await;
        core.learn.enabled = true;
        core.store
            .arm_periodic(
                Stage::Associate,
                "collection",
                crate::core::background::ASSOCIATE_TARGET,
                0,
            )
            .await
            .unwrap();
        let job = core.store.claim_job().await.unwrap().unwrap();
        core.learn.enabled = false;

        run_claimed(&core, job).await.unwrap();

        assert!(
            pending_run_after(&core, Stage::Associate).await.is_none(),
            "a sweep switched off armed itself anyway"
        );
    }

    #[tokio::test]
    async fn replay_comes_before_pursue() {
        // A sitting scored against the links this run folded in, not against
        // the last half-hour's. The pursuit sweep keeps its own period as a
        // floor; this is what pulls it forward.
        let mut core = test_core().await;
        core.learn.enabled = true;
        // Pursuit asleep on its own period, as it is for all but a moment of
        // every cycle.
        core.store
            .arm_periodic(
                Stage::Pursuit,
                "collection",
                crate::core::background::ASSOCIATE_TARGET,
                crate::store::now() + 3600,
            )
            .await
            .unwrap();
        core.store
            .arm_periodic(
                Stage::Associate,
                "collection",
                crate::core::background::ASSOCIATE_TARGET,
                0,
            )
            .await
            .unwrap();

        let job = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(job.stage, Stage::Associate);
        run_claimed(&core, job).await.unwrap();

        assert_eq!(
            pending_run_after(&core, Stage::Pursuit).await,
            Some(0),
            "the pursuit sweep was left asleep after the replay it depends on"
        );
    }

    #[tokio::test]
    async fn a_batch_embed_the_endpoint_rejects_is_retried_chunk_by_chunk() {
        let mut core = test_core().await;
        core.embedder = Arc::new(FakeEmbedder::rejecting("HTTP 413"));
        let src = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();
        // Run the queue up to and including the batch embed of this corpus.
        let mut guard = 0;
        while let Some(job) = core.store.claim_job().await.unwrap() {
            guard += 1;
            assert!(guard < 50, "queue did not reach the batch embed");
            let is_batch = job.stage == Stage::Embed && job.target_kind == "corpus";
            run_claimed(&core, job).await.unwrap();
            if is_batch {
                break;
            }
        }
        let per_chunk = core
            .store
            .pending_artifacts_for_corpus(&src.id)
            .await
            .unwrap();
        assert!(!per_chunk.is_empty());
        for c in per_chunk {
            assert!(
                core.store.live_job(Stage::Embed, &c.id).await.unwrap(),
                "per-chunk unit armed"
            );
        }
        assert!(
            !core.store.live_job(Stage::Embed, &src.id).await.unwrap(),
            "the batch unit is closed"
        );
    }

    /// A window the endpoint *refuses* used to be requeued at a fixed 32
    /// seconds forever, because the permanent arm passed the `MAX_ATTEMPTS`
    /// constant rather than the unit's own attempt count. Work the endpoint
    /// said no to would then poll it 675 times more often than work that
    /// merely failed to reach it — the wrong way round.
    #[tokio::test]
    async fn a_refused_window_backs_off_further_every_time_it_is_refused() {
        let mut core = test_core().await;
        core.synthesizer = Arc::new(crate::infer::fake::FakeSynthesizer::rejecting(
            "HTTP 400: context length exceeded",
        ));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        let delay = |core: &Core| {
            let control_pool = core.store.control.pool.clone();
            let id = out.id.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT run_after - ? FROM jobs
                      WHERE stage = 'segment_window' AND target_id LIKE ? || '%'",
                )
                .bind(crate::store::now())
                .bind(id)
                .fetch_one(&control_pool)
                .await
                .unwrap()
            }
        };

        // This measures one unit's backoff. Plan first so the window unit
        // exists, then clear everything else capture queued beside it, so
        // every iteration below claims the refusing window and nothing else.
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query("DELETE FROM jobs WHERE stage != 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        // Wind each refusal's delay back so the attempt budget is spent
        // without sleeping, and record how long the unit asked to wait.
        let mut gaps = Vec::new();
        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            let _ = run_one(&core).await;
            // Before the window's own unit is first claimed its `run_after` is
            // the zero this loop just wrote, which is not a refusal's delay.
            match delay(&core).await {
                g if g > 0 => gaps.push(g),
                _ => {}
            }
        }

        assert!(
            gaps.last() > gaps.first(),
            "a refusal must back off further each time, not poll a dead endpoint \
             at a fixed interval forever; gaps were {gaps:?}"
        );
        // The floor is still the ceiling of the ordinary budget, so a refusal
        // is never *cheaper* to retry than a failure to connect.
        assert!(
            gaps.iter().all(|g| *g >= backoff_secs(MAX_ATTEMPTS)),
            "gaps were {gaps:?}"
        );
        // And the document does not hang waiting on a window nobody will
        // take: its passages are captured and searchable, with the one
        // refused rewrite owed — partial, not failed.
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

    /// The instance-wide claim, and the thing it has to get right: the unit
    /// runs against the core belonging to whoever queued it.
    ///
    /// `Retention` because it leaves a trace in the tenant it ran for —
    /// `run_accounted` writes a `sweep_runs` row into that tenant's own
    /// database. A worker that resolved the wrong core would still complete
    /// the job; the row is what says which base it touched.
    #[tokio::test]
    async fn a_unit_runs_against_the_core_of_whoever_queued_it() {
        let (tenants, a, b, _dir) = crate::tenants::test_support::two_tenants().await;
        a.core
            .store
            .arm_periodic(Stage::Retention, "collection", "collection", 0)
            .await
            .unwrap();

        assert!(run_any(&tenants).await.unwrap(), "the job was claimed");

        let ran = |t: &crate::tenants::Tenant| {
            let store = t.core.store.clone();
            async move { store.sweep_runs_since(0, 10).await.unwrap().len() }
        };
        assert_eq!(
            ran(&a).await,
            1,
            "the sweep did not run for the tenant that armed it"
        );
        assert_eq!(ran(&b).await, 0, "another tenant's base was touched");
    }

    /// A row can outlive its user: the cascade takes the queue with the row,
    /// but a unit claimed a moment before the delete is already in a worker's
    /// hand. Retrying it can never succeed, so it is closed rather than failed.
    #[tokio::test]
    async fn a_job_for_a_deleted_user_is_dropped_rather_than_retried() {
        let (tenants, a, _b, _dir) = crate::tenants::test_support::two_tenants().await;
        a.core
            .store
            .enqueue(Stage::Embed, "corpus", "c-a")
            .await
            .unwrap();
        // The cascade would take the queue row with the user, which is the
        // ordinary case and leaves nothing to claim. What this reaches is the
        // narrower one the branch exists for: the row outliving its user
        // because the two deletes did not happen together. Foreign keys are
        // per-connection in SQLite, so one connection with them off is how
        // that state is reachable at all.
        {
            use sqlx::Executor;
            let mut conn = tenants.control().pool.acquire().await.unwrap();
            conn.execute("PRAGMA foreign_keys = OFF").await.unwrap();
            sqlx::query("DELETE FROM users WHERE subject = ?")
                .bind("sub-a")
                .execute(&mut *conn)
                .await
                .unwrap();
        }

        assert!(
            run_any(&tenants).await.unwrap(),
            "the orphaned unit was claimed and dealt with"
        );
        assert!(
            !run_any(&tenants).await.unwrap(),
            "it was closed, not left to be claimed again"
        );
    }

    /// The claim happens before the tenant is resolved, so a base that will
    /// not open leaves a row somebody has already taken. Left there, it is
    /// `running` with nobody holding it: no backoff, no `last_error`, one
    /// attempt spent, and nothing until the hourly `reclaim_stuck` — which
    /// during an outage is every unit in the queue, drained into a state a
    /// worker cannot see.
    #[tokio::test]
    async fn a_unit_whose_base_will_not_open_goes_back_on_the_queue() {
        use sqlx::Row;
        let (tenants, _dir) = crate::tenants::test_support::unopenable_tenants().await;
        tenants.control().provision("sub-a", None).await.unwrap();
        crate::store::jobs::enqueue_with(
            tenants.control(),
            "sub-a",
            Stage::Relate,
            "artifact",
            "art-1",
        )
        .await
        .unwrap();

        assert!(
            run_any(&tenants).await.is_err(),
            "the outage has to reach the worker loop, which is what makes it pause"
        );

        let row = sqlx::query("SELECT state, attempts, run_after, last_error FROM jobs")
            .fetch_one(&tenants.control().pool)
            .await
            .unwrap();
        assert_eq!(
            row.get::<String, _>("state"),
            "pending",
            "the claim was abandoned in `running`"
        );
        assert_eq!(row.get::<i64, _>("attempts"), 1);
        assert!(
            row.get::<i64, _>("run_after") > crate::store::now(),
            "requeued with no backoff at all"
        );
        assert!(
            row.get::<Option<String>, _>("last_error").is_some(),
            "nothing on the row says why it did not run"
        );
    }

    /// The `jobs` row a sweep left behind: when it next runs, and how many
    /// consecutive runs before this one found nothing.
    async fn pending_row(core: &Core, stage: Stage) -> (i64, i64) {
        sqlx::query_as(
            "SELECT run_after, empty_runs FROM jobs WHERE stage = ? AND state = 'pending'",
        )
        .bind(stage.as_str())
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap()
    }

    /// One sweep, as a worker runs it: the unit is closed, then re-armed.
    ///
    /// In that order and not the other, because the queue is keyed by
    /// `(stage, target)` — a re-arm before the close would upsert the very row
    /// the close then shuts, and the guard on the upsert says so by refusing to
    /// touch anything that is not already finished.
    async fn sweep_finds(core: &Core, stage: Stage, did_work: bool) {
        let id: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE stage = ?")
            .bind(stage.as_str())
            .fetch_optional(&core.store.control.pool)
            .await
            .unwrap()
            .unwrap_or(0);
        if id > 0 {
            core.store.complete_job(id).await.unwrap();
        }
        rearm_periodic_with(core, stage, "collection", did_work).await;
    }

    /// How long a re-armed unit has to wait, in whole seconds.
    ///
    /// Not on tokio's paused clock, though the pacing gate's tests are: a
    /// paused clock makes sqlx's pool time out acquiring its first connection,
    /// because the acquire timeout fires before any real work can happen. These
    /// tests never sleep, so a second of wall clock between arming and reading
    /// is the only imprecision, and one second against a period of minutes is
    /// not what any of them is about.
    /// A core on which `Retention` is a periodic unit at all. `test_core` has
    /// learning off and `retain_days` at zero, which is a base with nothing to
    /// expire and so no unit to schedule.
    async fn sweeping_core() -> Core {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core
    }

    async fn waits_about(core: &Core, stage: Stage, expected: i64) -> bool {
        let (run_after, _) = pending_row(core, stage).await;
        (run_after - crate::store::now()).abs_diff(expected) <= 2
    }

    /// A dormant tenant must not cost a wake-up an interval for ever. Nothing
    /// here costs a model call — a sweep with nothing to do makes none — so the
    /// backoff is proportionate to a wake-up, a file open and a few queries.
    #[tokio::test]
    async fn a_sweep_that_finds_nothing_waits_longer_each_time() {
        let core = sweeping_core().await;
        let base = crate::core::background::periodic_period(&core, Stage::Retention)
            .unwrap()
            .as_secs() as i64;

        for (run, expected) in [base, base * 2, base * 4].into_iter().enumerate() {
            sweep_finds(&core, Stage::Retention, false).await;
            assert!(
                waits_about(&core, Stage::Retention, expected).await,
                "empty run {} did not wait {expected}s",
                run + 1
            );
            let (_, empty) = pending_row(&core, Stage::Retention).await;
            assert_eq!(empty as usize, run + 1, "the empty runs were not counted");
        }
    }

    #[tokio::test]
    async fn the_wait_is_capped() {
        let core = sweeping_core().await;
        let cap = core.schedule.backoff_max_hours as i64 * 3600;
        for _ in 0..20 {
            sweep_finds(&core, Stage::Retention, false).await;
        }
        let (run_after, _) = pending_row(&core, Stage::Retention).await;
        assert!(
            run_after - crate::store::now() <= cap,
            "a quiet base backed off past its ceiling"
        );
    }

    #[tokio::test]
    async fn a_run_that_did_work_goes_back_to_the_configured_period() {
        let core = sweeping_core().await;
        let base = crate::core::background::periodic_period(&core, Stage::Retention)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..5 {
            sweep_finds(&core, Stage::Retention, false).await;
        }
        sweep_finds(&core, Stage::Retention, true).await;
        assert!(waits_about(&core, Stage::Retention, base).await);
        let (_, empty) = pending_row(&core, Stage::Retention).await;
        assert_eq!(empty, 0, "the count did not start over");
    }

    /// The reset comes free, and it is what makes the backoff safe rather than
    /// a firing rule: every producer already calls `arm_now`, so new data
    /// cancels the wait with no producer changes at all.
    #[tokio::test]
    async fn new_data_cancels_the_backoff() {
        let core = sweeping_core().await;
        for _ in 0..5 {
            sweep_finds(&core, Stage::Retention, false).await;
        }
        core.store
            .arm_now(Stage::Retention, "collection", "collection")
            .await
            .unwrap();
        let (run_after, empty) = pending_row(&core, Stage::Retention).await;
        assert_eq!(
            run_after, 0,
            "arm_now already pulls a sleeping unit forward"
        );
        assert_eq!(empty, 0);
    }

    /// The case the sentence above did not cover, and the one a busy base hits
    /// most: the capture lands while the sweep it concerns is running. That row
    /// is neither sleeping nor closed, so neither of `arm_now`'s guarded
    /// statements matched it and the count it was meant to clear survived — the
    /// sweep finished a moment later, read it, and re-armed at the doubled wait
    /// with the new data already in the base.
    #[tokio::test]
    async fn data_arriving_mid_sweep_cancels_the_backoff_too() {
        let core = sweeping_core().await;
        let base = crate::core::background::periodic_period(&core, Stage::Retention)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..5 {
            sweep_finds(&core, Stage::Retention, false).await;
        }

        // The worker is inside the unit: the row is `running`, which is neither
        // of the two states `arm_now` guards on. Set here rather than through
        // `claim_job`, which would not hand out a row still sleeping behind the
        // backoff this test just built.
        sqlx::query("UPDATE jobs SET state = 'running' WHERE stage = ?")
            .bind(Stage::Retention.as_str())
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        core.store
            .arm_now(Stage::Retention, "collection", "collection")
            .await
            .unwrap();

        // And the sweep finishes, having found nothing — it started before the
        // capture landed. This run still waits the plain period, because the
        // count of consecutive empty runs is about a base where nothing is
        // happening and something just did.
        sweep_finds(&core, Stage::Retention, false).await;
        assert!(waits_about(&core, Stage::Retention, base).await);
    }

    #[test]
    fn a_sweep_did_work_when_any_of_its_counts_moved() {
        // Read off the counts the account already writes, rather than each
        // sweep learning to say so: a report that gains a field keeps working.
        assert!(!did_work(r#"{"expired":0,"named":0}"#));
        assert!(did_work(r#"{"expired":0,"named":3}"#));
        assert!(
            !did_work("{}"),
            "a sweep that reported nothing found nothing"
        );
        assert!(
            !did_work("not json"),
            "an unreadable report is not a claim of work"
        );

        // A count one level down is a standing count and never work. Flat, the
        // retention sweep's `clusters` — how many gap clusters the base holds,
        // not how many this pass touched — claimed work on every run over any
        // base with a gap in it, which is precisely the dormant base the
        // backoff exists for.
        assert!(!did_work(r#"{"expired":0,"standing":{"clusters":3}}"#));
        assert!(did_work(r#"{"expired":1,"standing":{"clusters":3}}"#));

        let quiet = serde_json::to_string(&crate::jobs::retention::Report {
            standing: crate::jobs::retention::Standing { clusters: 12 },
            ..Default::default()
        })
        .unwrap();
        assert!(!did_work(&quiet), "{quiet}");

        // Same shape, and worse: `context::run` is a full recompute, so all
        // three of its standing counts are non-zero on every run over unchanged
        // data and the sweep could never report an empty run at all.
        let recomputed = serde_json::to_string(&crate::jobs::context::Report {
            standing: crate::jobs::context::Standing {
                events: 40,
                profiled: 6,
                clusters: 9,
            },
            cleared: 0,
        })
        .unwrap();
        assert!(!did_work(&recomputed), "{recomputed}");
        let cleared = serde_json::to_string(&crate::jobs::context::Report {
            cleared: 1,
            ..Default::default()
        })
        .unwrap();
        assert!(did_work(&cleared), "a profile that decayed away is work");

        // The consolidation sweep's repair passes had no field in its report,
        // so a tick that moved hundreds of pairs onto the artifacts that could
        // answer them still said `{"superseded":0,"judged":0}` — "no work" —
        // and doubled its backoff toward the 24-hour ceiling with the backlog
        // it was draining still there.
        let repairing = serde_json::to_string(&crate::jobs::consolidate::Outcome {
            repaired: 180,
            ..Default::default()
        })
        .unwrap();
        assert!(did_work(&repairing), "{repairing}");
    }

    #[tokio::test]
    async fn run_one_reports_when_the_queue_is_empty() {
        let core = test_core().await;
        assert!(!run_one(&core).await.unwrap(), "no jobs means no work done");
    }

    #[tokio::test]
    async fn draining_the_queue_takes_a_source_all_the_way_to_ready() {
        let core = test_core().await;
        let out = core
            .ingest("alpha para\n\nbeta para", "web", None)
            .await
            .unwrap();

        let mut guard = 0;
        while run_one(&core).await.unwrap() {
            guard += 1;
            assert!(guard < 50, "worker loop failed to terminate");
        }

        let src = core.store.get_corpus(&out.id).await.unwrap();
        assert_eq!(src.status, CorpusStatus::Ready);
        // Three vectors: the two synthesized artifacts, and the passage they
        // superseded — which keeps its vector, hidden from results by status.
        assert_eq!(core.vectors.count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn a_failing_stage_is_retried_then_gives_up_with_a_reason() {
        let mut core = test_core().await;
        core.synthesizer = Arc::new(crate::infer::fake::FakeSynthesizer::failing(
            "endpoint down",
        ));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        // Each attempt fails and pushes run_after forward; wind it back to
        // exercise the attempt budget without sleeping. The queue also holds
        // the capture's embed and follow-on units, so drive well past the
        // window's own budget rather than counting iterations against it.
        for _ in 0..40 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            let _ = run_one(&core).await;
        }

        // Verbatim-first: the capture survives the model in full — its
        // passages are the index — and only the rewrite is missing.
        let rows = core.store.artifacts_for_corpus(&out.id).await.unwrap();
        assert!(!rows.is_empty(), "the verbatim capture must survive");
        assert!(
            rows.iter()
                .all(|c| c.provenance == crate::store::artifacts::Provenance::Passage),
            "nothing synthesized can exist; the model never answered"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial,
            "captured and searchable, with the one window's rewrite owed"
        );
        // The window says why, so Ops can name the lines and the error rather
        // than only reporting that something did not work.
        let w = &core.store.segments_for_corpus(&out.id).await.unwrap()[0];
        assert_eq!(w.state, crate::store::segments::SegmentState::Failed);
        assert!(
            w.last_error.as_deref().is_some_and(|e| !e.is_empty()),
            "a failed window must carry the model's own error"
        );
    }

    #[tokio::test]
    async fn a_job_for_a_deleted_target_is_completed_not_retried_forever() {
        let core = test_core().await;
        core.store
            .enqueue(
                crate::store::jobs::Stage::Embed,
                "artifact",
                "does-not-exist",
            )
            .await
            .unwrap();
        run_one(&core).await.unwrap();
        assert!(
            core.store.claim_job().await.unwrap().is_none(),
            "a NotFound target must not stay in the queue"
        );
    }

    #[tokio::test]
    async fn concurrent_workers_drain_a_queue_without_duplicating_chunks() {
        // The real deployment runs several workers against one database. If
        // claiming or the segment replace-step were not safe, this would
        // produce duplicated or missing chunks.
        let core = test_core().await;
        let mut ids = Vec::new();
        for i in 0..12 {
            ids.push(
                core.ingest(
                    &format!("doc {i} para one\n\ndoc {i} para two"),
                    "web",
                    None,
                )
                .await
                .unwrap()
                .id,
            );
        }

        let mut handles = Vec::new();
        for _ in 0..4 {
            let core = core.clone();
            handles.push(tokio::spawn(async move {
                while run_one(&core).await.unwrap_or(false) {}
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        for id in &ids {
            let live = core
                .store
                .artifacts_for_corpus(id)
                .await
                .unwrap()
                .into_iter()
                .filter(|c| c.in_results())
                .count();
            // Two synthesized artifacts plus the joint passage supersession
            // leaves standing (no single artifact majority-covers it).
            assert_eq!(live, 3, "source {id} has {live} live chunks");
            assert_eq!(
                core.store.get_corpus(id).await.unwrap().status,
                CorpusStatus::Ready
            );
        }
        // Per source: two synthesized artifacts plus the superseded passage's
        // kept vector.
        assert_eq!(core.vectors.count().await.unwrap(), 36);
    }
}
