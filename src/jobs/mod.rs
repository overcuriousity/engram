pub mod embed;
pub mod segment;

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::jobs::{MAX_ATTEMPTS, Stage};
use std::time::Duration;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A job still marked `running` after this long belonged to a process that died.
pub const STUCK_AFTER_SECS: i64 = 600;

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
    let _guard = span.enter();

    let result = match (job.stage, job.target_kind.as_str()) {
        (Stage::Segment | Stage::Enrich, _) => segment::run(core, &job.target_id).await,
        // Embedding is batched per source; the per-chunk path is for edits,
        // for oversize splits, and for isolating a chunk the batch chokes on.
        (Stage::Embed, "source") => embed::run_source(core, &job.target_id).await,
        (Stage::Embed, _) => embed::run(core, &job.target_id).await,
    };

    match result {
        Ok(()) => {
            core.store.complete_job(job.id).await?;
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
                // Out of attempts against the chunker. Only the windows that
                // never finished are split structurally; the rest keep the
                // segmentation they already earned.
                (Stage::Segment, _) if exhausted => {
                    tracing::warn!(error = %e, "segmentation exhausted retries; falling back per window");
                    match segment::fallback_pending_windows(core, &job.target_id, &e.to_string())
                        .await
                    {
                        Ok(()) => {
                            core.store.complete_job(job.id).await?;
                        }
                        Err(fe) => {
                            core.store
                                .fail_job(job.id, job.attempts, &fe.to_string())
                                .await?;
                        }
                    }
                }
                // A whole source failing together usually means the endpoint is
                // down, but it can also be one chunk the embedder rejects.
                // Retrying chunk by chunk isolates the culprit either way.
                (Stage::Embed, "source") if exhausted => {
                    tracing::warn!(error = %e, "batch embedding exhausted retries; retrying chunk by chunk");
                    match embed::split_into_chunk_jobs(core, &job.target_id).await {
                        Ok(()) => {
                            core.store.complete_job(job.id).await?;
                        }
                        Err(fe) => {
                            core.store
                                .fail_job(job.id, job.attempts, &fe.to_string())
                                .await?;
                        }
                    }
                }
                _ => {
                    tracing::warn!(error = %e, "job failed; will retry");
                    core.store
                        .fail_job(job.id, job.attempts, &e.to_string())
                        .await?;
                    if job.stage == Stage::Embed && exhausted {
                        core.store.mark_embed_failed(&job.target_id).await?;
                        if let Ok(c) = core.store.get_chunk(&job.target_id).await {
                            embed::settle_source(core, &c.source_id).await?;
                        }
                    }
                }
            }
            Ok(true)
        }
        Err(e) => {
            tracing::error!(error = %e, "job failed permanently");
            core.store
                .fail_job(job.id, MAX_ATTEMPTS, &e.to_string())
                .await?;
            Ok(true)
        }
    }
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
    use crate::core::test_support::{test_core, test_core_with_failing_chunker};
    use crate::store::jobs::MAX_ATTEMPTS;
    use crate::store::sources::SourceStatus;

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

        let src = core.store.get_source(&out.id).await.unwrap();
        assert_eq!(src.status, SourceStatus::Ready);
        assert_eq!(core.vectors.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_failing_stage_is_retried_then_falls_back() {
        let core = test_core_with_failing_chunker().await;
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

        let chunks = core.store.chunks_for_source(&out.id).await.unwrap();
        assert_eq!(
            chunks.len(),
            2,
            "exhausting retries must not lose the source"
        );
        assert_eq!(
            core.store.get_source(&out.id).await.unwrap().status,
            SourceStatus::Partial
        );
    }

    #[tokio::test]
    async fn a_job_for_a_deleted_target_is_completed_not_retried_forever() {
        let core = test_core().await;
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "chunk", "does-not-exist")
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
            let chunks = core.store.chunks_for_source(id).await.unwrap();
            assert_eq!(chunks.len(), 2, "source {id} has {} chunks", chunks.len());
            assert_eq!(
                core.store.get_source(id).await.unwrap().status,
                SourceStatus::Ready
            );
        }
        assert_eq!(core.vectors.count().await.unwrap(), 24);
    }
}
