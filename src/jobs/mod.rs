pub mod associate;
pub mod condense;
pub mod consolidate;
pub mod context;
pub mod dedupe;
pub mod describe;
pub mod embed;
pub mod extract;
pub mod gaps;
pub mod judgement;
pub mod merge;
pub mod observe;
pub mod passages;
pub mod probe;
pub mod promote;
pub mod pursuit;
pub mod reap;
pub mod reconcile;
pub mod refetch;
pub mod relate;
pub mod remind;
pub mod retention;
pub mod retract;
pub mod sleep;
pub mod synthesize;
pub mod tune;
pub mod webpush;
pub mod window;

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::jobs::{Job, MAX_ATTEMPTS, Stage};
use std::time::Duration;
use tracing::Instrument;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A job still marked `running` after this long belonged to a process that died.
pub const STUCK_AFTER_SECS: i64 = 600;

/// `supersede` refuses an inactive side, and a status changing since the caller read it is a race,
/// not a failure of the sweep. Returns whether it happened.
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

/// Claim one unit from the instance-wide queue and run it against the core its row names.
/// A worker never holds a tenant across two units, which round-robins users without a scheduler.
pub async fn run_any(tenants: &crate::tenants::Tenants) -> Result<bool> {
    // Shut, the generating stages are passed over (`Stage::calls_a_generator`); a server never
    // shuts it.
    let held: &[Stage] = if tenants
        .generation()
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        &[]
    } else {
        &Stage::GENERATORS
    };
    let Some((subject, job)) = tenants.control().claim_job_holding(held).await? else {
        return Ok(false);
    };
    let core = match tenants.get(&subject).await {
        Ok(t) => t.core,
        // The user was deleted after the claim; retrying can never succeed.
        Err(Error::NotFound) => {
            tracing::info!(subject = %subject, "queue row for a user that no longer exists; dropping");
            tenants.control().complete_job(job.id).await?;
            return Ok(true);
        }
        // A fault of this moment, not of the row. The claim already happened, so put the unit back
        // at the ordinary backoff rather than leave it `running` until `reclaim_stuck`.
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

/// Claim and run at most one of this tenant's jobs; false when their queue is empty.
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
    // `.instrument`, not `span.enter()`: a guard held across `.await` does not travel with the
    // future.
    let ran = run_claimed(core, job).instrument(span).await;
    core.live.changed();
    ran
}

async fn run_claimed(core: &Core, job: Job) -> Result<bool> {
    // `did_work` governs only the next periodic run, so only a sweep sets it.
    let mut did_work = false;
    let result = match (job.stage, job.target_kind.as_str()) {
        (Stage::Synthesize, _) => synthesize::plan(core, &job.target_id).await,
        // Batched per source; the per-chunk path is for edits, oversize splits and isolating a bad
        // chunk.
        (Stage::Embed, "corpus") => embed::run_corpus(core, &job.target_id).await,
        (Stage::Embed, _) => embed::run(core, &job.target_id).await,
        (Stage::LinkJudge, _) => associate::judge(core, &job.target_id).await,
        (Stage::SegmentWindow, _) => window::run(core, &job.target_id).await,
        (Stage::Title, _) => synthesize::run_title(core, &job.target_id).await,
        (Stage::Dedupe, _) => dedupe::run(core, &job.target_id).await,
        (Stage::Relate, _) => relate::run(core, &job.target_id).await,
        (Stage::Probe, _) => probe::run(core, &job.target_id).await,
        (Stage::Condense, _) => condense::run(core, &job.target_id).await,
        (Stage::Describe, _) => describe::run(core, &job.target_id).await,
        (Stage::Extract, _) => extract::run(core, &job.target_id).await,
        (Stage::Fetch, _) => refetch::run(core, &job.target_id).await,
        (Stage::Generate, _) => pursuit::generate(core, &job.target_id).await,
        // The sweeps look at the whole collection and ignore the target.
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
            // After completing, never before: the queue is keyed by (stage, target), so a handler
            // re-arming itself would upsert the row `complete_job` then closes.
            if job.stage == Stage::Embed && job.target_kind == "corpus" {
                embed::rearm_if_more(core, &job.target_id).await?;
            }
            // Same rule: `arm_at` refuses a `running` row. Logged, not propagated: `complete_job`
            // has committed, and `arm_missing_periodic` repairs a lost arming.
            if job.stage == Stage::Remind
                && let Err(e) = core.store.rearm_remind().await
            {
                tracing::warn!(error = %e, "could not re-arm the reminder unit; the repair pass picks it up");
            }
            rearm_periodic(core, &job, did_work).await;
            arm_successor(core, &job).await;
            Ok(true)
        }
        // The target was deleted while the job waited; retrying can never succeed.
        Err(Error::NotFound) => {
            tracing::info!("target no longer exists; dropping job");
            core.store.complete_job(job.id).await?;
            Ok(true)
        }
        Err(e) if e.retryable() => {
            let exhausted = job.attempts >= MAX_ATTEMPTS;
            match (job.stage, job.target_kind.as_str()) {
                // A push sent but not recorded: retrying re-delivers it. Closed without re-arming,
                // so the unit returns through `arm_missing_periodic`, not the past instant.
                (Stage::Remind, _) if exhausted => {
                    tracing::warn!(error = %e, "could not record a push after every retry; standing the unit down until the repair pass");
                    core.store.complete_job(job.id).await?;
                }
                // A name is decoration, and an unjudged pair stays pending for a later sweep;
                // everything else stays queued, as it carries knowledge.
                (Stage::Dedupe | Stage::Title, _) if exhausted => {
                    tracing::warn!(error = %e, stage = job.stage.as_str(), "giving up on this unit for now");
                    core.store.complete_job(job.id).await?;
                }
                // The photo is stored, but a corpus in flight forever is a lie. An unconfigured
                // role is a wait, not a failure.
                (Stage::Describe, _) if exhausted && core.describer.is_some() => {
                    tracing::warn!(error = %e, "could not read this image; parking it");
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // No role guard: extraction needs nothing configured.
                (Stage::Extract, _) if exhausted => {
                    tracing::warn!(error = %e, "could not extract this PDF; parking it");
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // A whole source failing is usually the endpoint, but can be one chunk the embedder
                // rejects.
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
                // Refused as a batch does not mean refused as chunks.
                (Stage::Embed, "corpus") => split_or_fail(core, &job, &e).await?,
                (Stage::Embed, _) => settle_failed_artifact(core, &job, &e).await?,
                (Stage::Describe | Stage::Extract, _) => {
                    park_failed_if_still_there(core, job.stage, &job.target_id, &e).await?;
                    core.store.complete_job(job.id).await?;
                }
                // A refusal is the site's answer today, so it is asked again until
                // `refetch::ATTEMPTS`; then the capture stays a searchable link with the reason.
                (Stage::Fetch, _) => {
                    if job.attempts >= refetch::ATTEMPTS {
                        match core.give_up_on_link(&job.target_id, &e.to_string()).await {
                            Ok(()) | Err(Error::NotFound) => {}
                            Err(err) => return Err(err),
                        }
                        core.store.complete_job(job.id).await?;
                    } else {
                        core.store
                            .fail_job(job.id, job.attempts, &e.to_string())
                            .await?;
                    }
                }
                // Floored at `MAX_ATTEMPTS`: later refusals keep backing off rather than pinning
                // one delay.
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

/// Recorded here, failures included, whatever happens to the unit afterwards: a failed sweep's
/// retry is the same run.
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
        // A sweep arriving here unaccounted for is worse than a row saying so.
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

/// Any non-zero *flat* number in the report is work. Nested counts are standing counts
/// (`retention::Standing`, `context::Standing`), so a dormant base still backs off. An unreadable
/// report is not work.
fn did_work(detail: &str) -> bool {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(detail)
    else {
        return false;
    };
    map.values()
        .filter_map(serde_json::Value::as_f64)
        .any(|n| n != 0.0)
}

fn detail<T: serde::Serialize>(report: T) -> Result<String> {
    Ok(serde_json::to_string(&report).unwrap_or_else(|_| "{}".into()))
}

/// Arms a finished sweep one interval out. A failed sweep needs nothing: `fail_job` leaves it
/// pending behind a backoff. A sweep switched off since arming stays closed.
async fn rearm_periodic(core: &Core, job: &Job, did_work: bool) {
    rearm_periodic_with(core, job.stage, &job.target_id, did_work).await;
}

/// The period after work, doubled per consecutive empty run up to `schedule.backoff_max_hours`.
/// `arm_now` resets it, so new data cancels the wait without any producer change.
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
    // `empty - 1` doublings, with the shift bounded so a long-dormant base cannot wrap it short.
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

/// Replay before pursue: a sitting scores against the links this run folded in. Pursuit keeps its
/// own period as a floor.
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

/// Tolerates the corpus being deleted between the failed read and this write; otherwise the job
/// stays `running` until `reclaim_stuck`, only to lose the race again.
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

/// Hand a batch embed that will not go through to per-chunk units; the batch stays armed if even
/// that fails.
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

/// Marks one chunk the embedder will not take; the corpus settles to `partial` if it was the last.
async fn settle_failed_artifact(core: &Core, job: &Job, e: &Error) -> Result<()> {
    // Kept armed at the backoff ceiling: no terminal state (`fail_job`).
    core.store
        .fail_job(job.id, job.attempts.max(MAX_ATTEMPTS), &e.to_string())
        .await?;
    core.store.mark_embed_failed(&job.target_id).await?;
    // A merged artifact belongs to no corpus.
    if let Ok(c) = core.store.get_artifact(&job.target_id).await
        && let Some(corpus_id) = c.corpus_id.as_deref()
    {
        embed::settle_corpus(core, corpus_id).await?;
    }
    Ok(())
}

pub struct Worker;

impl Worker {
    /// One pool for the instance: `server.workers` is the admission point in front of one set of
    /// inference endpoints, whatever the number of users.
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
    /// Run the queue dry.
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

    #[test]
    fn a_sweeps_standing_counts_are_not_work() {
        use crate::jobs::retention::{Report, Standing};

        // Every number here says what the base holds, not what this pass did.
        let quiet = Report {
            standing: Standing {
                clusters: 7,
                rehearsed: 42,
                interference: 3,
            },
            ..Default::default()
        };
        assert!(
            !did_work(&detail(&quiet).unwrap()),
            "a quiet base reported work, so the backoff never engages"
        );

        for busy in [
            Report {
                moved: 1,
                ..quiet.clone()
            },
            Report {
                retired: 1,
                ..quiet.clone()
            },
            Report {
                condensed: 1,
                ..quiet.clone()
            },
            Report {
                integrated: 1,
                ..quiet.clone()
            },
            Report {
                gave_up: 1,
                ..quiet.clone()
            },
        ] {
            assert!(
                did_work(&detail(&busy).unwrap()),
                "a pass that changed something reported none: {busy:?}"
            );
        }
    }

    async fn pending_run_after(core: &Core, stage: Stage) -> Option<i64> {
        sqlx::query_scalar!(
            "SELECT run_after FROM jobs WHERE stage = ? AND state = 'pending'",
            stage.as_str()
        )
        .fetch_optional(&core.store.control.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_sweep_that_ran_says_what_it_did() {
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
        let n: i64 = sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE stage = 'associate'")
            .fetch_one(&core.store.control.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn a_sweep_that_failed_is_still_going_to_run_again() {
        // `fail_job` leaves the row pending behind a backoff; this asserts nothing closes it
        // instead.
        let mut core = test_core().await;
        core.learn.enabled = true;
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

        let state: String = sqlx::query_scalar!("SELECT state FROM jobs WHERE id = ?", id)
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
        let mut core = test_core().await;
        core.learn.enabled = true;
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

    #[tokio::test]
    async fn a_refused_window_backs_off_further_every_time_it_is_refused() {
        let mut core = test_core().await;
        core.synthesizer = Some(Arc::new(crate::infer::fake::FakeSynthesizer::rejecting(
            "HTTP 400: context length exceeded",
        )));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        let delay = |core: &Core| {
            let control_pool = core.store.control.pool.clone();
            let id = out.id.clone();
            async move {
                sqlx::query_scalar!(
                    r#"SELECT run_after - ? AS "delay!: i64" FROM jobs
                      WHERE stage = 'segment_window' AND target_id LIKE ? || '%'"#,
                    crate::store::now(),
                    id
                )
                .fetch_one(&control_pool)
                .await
                .unwrap()
            }
        };

        // Clear everything but the window unit, so every iteration claims the refusing window.
        crate::jobs::synthesize::plan(&core, &out.id).await.unwrap();
        sqlx::query!("DELETE FROM jobs WHERE stage != 'segment_window'")
            .execute(&core.store.control.pool)
            .await
            .unwrap();

        let mut gaps = Vec::new();
        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            let _ = run_one(&core).await;
            // Before its first claim the unit's `run_after` is the zero just written, not a
            // refusal's delay.
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
        assert!(
            gaps.iter().all(|g| *g >= backoff_secs(MAX_ATTEMPTS)),
            "gaps were {gaps:?}"
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Partial
        );
    }

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

    #[tokio::test]
    async fn a_job_for_a_deleted_user_is_dropped_rather_than_retried() {
        let (tenants, a, _b, _dir) = crate::tenants::test_support::two_tenants().await;
        a.core
            .store
            .enqueue(Stage::Embed, "corpus", "c-a")
            .await
            .unwrap();
        // Foreign keys are per-connection in SQLite: one connection with them off is how a row
        // outlives its user.
        {
            use sqlx::Executor;
            let mut conn = tenants.control().pool.acquire().await.unwrap();
            conn.execute("PRAGMA foreign_keys = OFF").await.unwrap();
            sqlx::query!("DELETE FROM users WHERE subject = ?", "sub-a")
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

    #[tokio::test]
    async fn a_unit_whose_base_will_not_open_goes_back_on_the_queue() {
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

        let row = sqlx::query!("SELECT state, attempts, run_after, last_error FROM jobs")
            .fetch_one(&tenants.control().pool)
            .await
            .unwrap();
        assert_eq!(row.state, "pending", "the claim was abandoned in `running`");
        assert_eq!(row.attempts, 1);
        assert!(
            row.run_after > crate::store::now(),
            "requeued with no backoff at all"
        );
        assert!(
            row.last_error.is_some(),
            "nothing on the row says why it did not run"
        );
    }

    async fn pending_row(core: &Core, stage: Stage) -> (i64, i64) {
        sqlx::query!(
            "SELECT run_after, empty_runs FROM jobs WHERE stage = ? AND state = 'pending'",
            stage.as_str()
        )
        .fetch_one(&core.store.control.pool)
        .await
        .map(|r| (r.run_after, r.empty_runs))
        .unwrap()
    }

    async fn sweep_finds(core: &Core, stage: Stage, did_work: bool) {
        let id: i64 = sqlx::query_scalar!("SELECT id FROM jobs WHERE stage = ?", stage.as_str())
            .fetch_optional(&core.store.control.pool)
            .await
            .unwrap()
            .unwrap_or(0);
        if id > 0 {
            core.store.complete_job(id).await.unwrap();
        }
        rearm_periodic_with(core, stage, "collection", did_work).await;
    }

    async fn sweeping_core() -> Core {
        let mut core = test_core().await;
        core.learn.enabled = true;
        core
    }

    async fn waits_about(core: &Core, stage: Stage, expected: i64) -> bool {
        let (run_after, _) = pending_row(core, stage).await;
        (run_after - crate::store::now()).abs_diff(expected) <= 2
    }

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

    #[tokio::test]
    async fn data_arriving_mid_sweep_cancels_the_backoff_too() {
        let core = sweeping_core().await;
        let base = crate::core::background::periodic_period(&core, Stage::Retention)
            .unwrap()
            .as_secs() as i64;
        for _ in 0..5 {
            sweep_finds(&core, Stage::Retention, false).await;
        }

        // Set directly: `claim_job` would not hand out a row still sleeping behind the backoff.
        sqlx::query!(
            "UPDATE jobs SET state = 'running' WHERE stage = ?",
            Stage::Retention.as_str()
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        core.store
            .arm_now(Stage::Retention, "collection", "collection")
            .await
            .unwrap();

        sweep_finds(&core, Stage::Retention, false).await;
        assert!(waits_about(&core, Stage::Retention, base).await);
    }

    #[test]
    fn a_sweep_did_work_when_any_of_its_counts_moved() {
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

        assert!(!did_work(r#"{"expired":0,"standing":{"clusters":3}}"#));
        assert!(did_work(r#"{"expired":1,"standing":{"clusters":3}}"#));

        let quiet = serde_json::to_string(&crate::jobs::retention::Report {
            standing: crate::jobs::retention::Standing {
                clusters: 12,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert!(!did_work(&quiet), "{quiet}");

        let slept = serde_json::to_string(&crate::jobs::retention::Report {
            integrated: 500,
            standing: crate::jobs::retention::Standing {
                rehearsed: 500,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert!(did_work(&slept), "{slept}");

        let rehearsed_only = serde_json::to_string(&crate::jobs::retention::Report {
            standing: crate::jobs::retention::Standing {
                rehearsed: 500,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert!(!did_work(&rehearsed_only), "{rehearsed_only}");

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
        // The superseded passage keeps its vector, hidden from results by status.
        assert_eq!(core.vectors.count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn a_failing_stage_is_retried_then_gives_up_with_a_reason() {
        let mut core = test_core().await;
        core.synthesizer = Some(Arc::new(crate::infer::fake::FakeSynthesizer::failing(
            "endpoint down",
        )));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        // The queue also holds the capture's other units, so drive well past the window's own
        // budget.
        for _ in 0..40 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            let _ = run_one(&core).await;
        }

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
            // No single artifact majority-covers the joint passage, so it stays live.
            assert_eq!(live, 3, "source {id} has {live} live chunks");
            assert_eq!(
                core.store.get_corpus(id).await.unwrap().status,
                CorpusStatus::Ready
            );
        }
        assert_eq!(core.vectors.count().await.unwrap(), 36);
    }
}
