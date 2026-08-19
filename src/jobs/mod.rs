pub mod associate;
pub mod consolidate;
pub mod dedupe;
pub mod describe;
pub mod embed;
pub mod extract;
pub mod gaps;
pub mod merge;
pub mod passages;
pub mod promote;
pub mod pursuit;
pub mod reconcile;
pub mod relate;
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

/// Claim and run at most one job. Returns false when the queue is empty, which
/// is the loop's signal to sleep.
pub async fn run_one(core: &Core) -> Result<bool> {
    let Some(job) = core.store.claim_job().await? else {
        return Ok(false);
    };

    let span = tracing::info_span!(
        "job",
        id = job.id,
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

/// The role a stage cannot run without. `Synthesize` is deliberately absent:
/// at `off` it is the verbatim capture path and needs nothing.
fn needs_model(stage: Stage) -> Option<&'static str> {
    match stage {
        Stage::SegmentWindow
        | Stage::Title
        | Stage::Dedupe
        | Stage::LinkJudge
        | Stage::Generate => Some("synthesize"),
        _ => None,
    }
}

async fn run_claimed(core: &Core, job: Job) -> Result<bool> {
    // A unit that needs a model the configuration does not have can never run.
    // Close it with a reason rather than retrying to the ceiling for ever —
    // a base captured at `eager`, then reconfigured, has rows like this.
    if let Some(role) = needs_model(job.stage)
        && !core.synthesizes()
    {
        tracing::warn!(
            stage = job.stage.as_str(),
            "no [infer.{role}] configured; dropping the unit"
        );
        core.store.complete_job(job.id).await?;
        return Ok(true);
    }
    let result = match (job.stage, job.target_kind.as_str()) {
        (Stage::Synthesize | Stage::Enrich, _) => synthesize::plan(core, &job.target_id).await,
        // Embedding is batched per source; the per-chunk path is for edits,
        // for oversize splits, and for isolating a chunk the batch chokes on.
        (Stage::Embed, "corpus") => embed::run_corpus(core, &job.target_id).await,
        (Stage::Embed, _) => embed::run(core, &job.target_id).await,
        // The sweep looks at the whole collection, so it ignores the target.
        (Stage::Consolidate, _) => consolidate::run(core).await.map(|_| ()),
        // The sweep looks at the whole collection, so it ignores the target.
        (Stage::Associate, _) => associate::run(core).await,
        (Stage::LinkJudge, _) => associate::judge(core, &job.target_id).await,
        (Stage::SegmentWindow, _) => window::run(core, &job.target_id).await,
        (Stage::Title, _) => synthesize::run_title(core, &job.target_id).await,
        (Stage::Dedupe, _) => dedupe::run(core, &job.target_id).await,
        (Stage::Relate, _) => relate::run(core, &job.target_id).await,
        (Stage::Describe, _) => describe::run(core, &job.target_id).await,
        (Stage::Extract, _) => extract::run(core, &job.target_id).await,
        (Stage::Pursuit, _) => pursuit::run(core).await.map(|_| ()),
        (Stage::Generate, _) => pursuit::generate(core, &job.target_id).await,
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
    pub fn spawn(
        core: Core,
        workers: usize,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        (0..workers.max(1))
            .map(|n| {
                let core = core.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    tracing::info!(worker = n, "worker started");
                    loop {
                        tokio::select! {
                            _ = shutdown.changed() => {
                                if *shutdown.borrow() { break; }
                            }
                            worked = run_one(&core) => {
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
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::infer::fake::FakeEmbedder;
    use crate::store::corpora::CorpusStatus;
    use crate::store::jobs::{MAX_ATTEMPTS, backoff_secs};
    use std::sync::Arc;

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
        core.synthesizer = Some(Arc::new(crate::infer::fake::FakeSynthesizer::rejecting(
            "HTTP 400: context length exceeded",
        )));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        let delay = |core: &Core| {
            let pool = core.store.pool.clone();
            let id = out.id.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT run_after - ? FROM jobs
                      WHERE stage = 'segment_window' AND target_id LIKE ? || '%'",
                )
                .bind(crate::store::now())
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };

        // Wind each refusal's delay back so the attempt budget is spent
        // without sleeping, and record how long the unit asked to wait.
        let mut gaps = Vec::new();
        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
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
        // And the document does not hang waiting on a window nobody will take.
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed
        );
    }

    #[tokio::test]
    async fn a_model_stage_job_with_no_model_is_closed_not_retried() {
        // A dedupe unit left over from an `eager` base, reconfigured to run
        // with no [infer.synthesize]: there is nothing that could ever run it,
        // so it settles with a reason rather than sitting at the backoff
        // ceiling for ever.
        let mut core = test_core().await;
        core.synthesizer = None;
        core.judge = None;
        core.store
            .enqueue(Stage::Dedupe, "pair", "p1")
            .await
            .unwrap();
        assert!(run_one(&core).await.unwrap(), "the job was claimed");
        assert!(
            !core.store.live_job(Stage::Dedupe, "p1").await.unwrap(),
            "the unit is still armed"
        );
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
        assert_eq!(core.vectors.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_failing_stage_is_retried_then_gives_up_with_a_reason() {
        let mut core = test_core().await;
        core.synthesizer = Some(Arc::new(crate::infer::fake::FakeSynthesizer::failing(
            "endpoint down",
        )));
        let out = core.ingest("alpha\n\nbeta", "web", None).await.unwrap();

        // Each attempt fails and pushes run_after forward; wind it back to
        // exercise the attempt budget without sleeping.
        for _ in 0..=MAX_ATTEMPTS {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await
                .unwrap();
            let _ = run_one(&core).await;
        }

        // The model is a hard dependency: a source it never segmented has no
        // chunks rather than paragraphs split on blank lines.
        assert!(
            core.store
                .artifacts_for_corpus(&out.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            core.store.get_corpus(&out.id).await.unwrap().status,
            CorpusStatus::Failed
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
            let chunks = core.store.artifacts_for_corpus(id).await.unwrap();
            assert_eq!(chunks.len(), 2, "source {id} has {} chunks", chunks.len());
            assert_eq!(
                core.store.get_corpus(id).await.unwrap().status,
                CorpusStatus::Ready
            );
        }
        assert_eq!(core.vectors.count().await.unwrap(), 24);
    }
}
