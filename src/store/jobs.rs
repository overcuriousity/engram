use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

pub const MAX_ATTEMPTS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Synthesize,
    Enrich,
    Embed,
    /// The periodic consolidation sweep. Its target is the collection rather
    /// than any one corpus, so there is exactly one of these in the queue at a
    /// time.
    Consolidate,
}

impl Stage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::Synthesize => "synthesize",
            Stage::Enrich => "enrich",
            Stage::Embed => "embed",
            Stage::Consolidate => "consolidate",
        }
    }
    pub fn parse(s: &str) -> Option<Stage> {
        match s {
            "synthesize" => Some(Stage::Synthesize),
            "enrich" => Some(Stage::Enrich),
            "embed" => Some(Stage::Embed),
            "consolidate" => Some(Stage::Consolidate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub stage: Stage,
    pub target_kind: String,
    pub target_id: String,
    pub attempts: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedJob {
    pub id: i64,
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub last_error: Option<String>,
}

/// 2s, 4s, 8s, 16s, 32s ... capped at five minutes. An endpoint that is down
/// stays down for minutes, not milliseconds, and a tight retry loop against a
/// dead inference server is just noise in the log.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.clamp(1, 16) as u32;
    2i64.saturating_pow(exp).min(300)
}

impl Store {
    pub async fn enqueue(&self, stage: Stage, target_kind: &str, target_id: &str) -> Result<()> {
        // Idempotent per (stage, target). A conflicting row that already
        // finished or failed is re-armed, which is exactly what a manual
        // reprocess needs.
        sqlx::query(
            "INSERT INTO jobs (stage, target_kind, target_id, state, attempts, run_after, created_at)
             VALUES (?, ?, ?, 'pending', 0, 0, ?)
             ON CONFLICT(stage, target_id) DO UPDATE SET
               state = 'pending', attempts = 0, run_after = 0, last_error = NULL,
               claimed_at = NULL, created_at = excluded.created_at",
        )
        .bind(stage.as_str())
        .bind(target_kind)
        .bind(target_id)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomic claim. The UPDATE ... WHERE id = (SELECT ...) RETURNING form runs
    /// as one statement under SQLite's write lock, so two workers can never
    /// take the same row.
    pub async fn claim_job(&self) -> Result<Option<Job>> {
        let row = sqlx::query(
            "UPDATE jobs
                SET state = 'running', claimed_at = ?, attempts = attempts + 1
              WHERE id = (
                SELECT id FROM jobs
                 WHERE state = 'pending' AND run_after <= ?
                 ORDER BY id LIMIT 1
              )
              RETURNING id, stage, target_kind, target_id, attempts",
        )
        .bind(now())
        .bind(now())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| Job {
            id: r.get("id"),
            stage: Stage::parse(r.get::<String, _>("stage").as_str()).unwrap_or(Stage::Synthesize),
            target_kind: r.get("target_kind"),
            target_id: r.get("target_id"),
            attempts: r.get("attempts"),
        }))
    }

    pub async fn complete_job(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = 'done', last_error = NULL, claimed_at = NULL WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        if attempts >= MAX_ATTEMPTS {
            sqlx::query(
                "UPDATE jobs SET state = 'failed', last_error = ?, claimed_at = NULL WHERE id = ?",
            )
            .bind(err)
            .bind(id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE jobs SET state = 'pending', run_after = ?, last_error = ?, claimed_at = NULL WHERE id = ?",
            )
            .bind(now() + backoff_secs(attempts))
            .bind(err)
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Rows left 'running' by a crashed process. Called once at startup.
    pub async fn reclaim_stuck(&self, older_than_secs: i64) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE jobs SET state = 'pending', run_after = 0, claimed_at = NULL
              WHERE state = 'running' AND claimed_at IS NOT NULL AND claimed_at < ?",
        )
        .bind(now() - older_than_secs)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn job_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query("SELECT state, COUNT(*) AS n FROM jobs GROUP BY state")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| (r.get("state"), r.get("n"))).collect())
    }

    pub async fn failed_jobs(&self, limit: i64) -> Result<Vec<FailedJob>> {
        let rows = sqlx::query(
            "SELECT id, stage, target_id, attempts, last_error FROM jobs
              WHERE state = 'failed' ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| FailedJob {
                id: r.get("id"),
                stage: r.get("stage"),
                target_id: r.get("target_id"),
                attempts: r.get("attempts"),
                last_error: r.get("last_error"),
            })
            .collect())
    }

    /// How long the longest-waiting pending job has been queued, in seconds.
    ///
    /// Measured from `created_at`, not `run_after`: a job that was never
    /// delayed has `run_after = 0`, which would report seconds-since-epoch.
    pub async fn oldest_pending_age(&self) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT MIN(created_at) AS oldest FROM jobs WHERE state = 'pending'")
            .fetch_one(&self.pool)
            .await?;
        let oldest: Option<i64> = row.get("oldest");
        Ok(oldest.map(|t| (now() - t).max(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn backoff_doubles_then_caps_at_five_minutes() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(9), 300, "must cap, not grow unbounded");
        assert_eq!(backoff_secs(100), 300);
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_per_stage_and_target() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        assert!(s.claim_job().await.unwrap().is_some());
        assert!(
            s.claim_job().await.unwrap().is_none(),
            "duplicate enqueue created a second job"
        );
    }

    #[tokio::test]
    async fn a_job_is_claimed_exactly_once() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();

        let a = s.claim_job().await.unwrap();
        let b = s.claim_job().await.unwrap();
        assert!(a.is_some());
        assert!(b.is_none(), "two workers claimed the same job");
        assert_eq!(a.unwrap().attempts, 1, "claiming must count the attempt");
    }

    /// The sequential test above cannot prove atomicity: `Store::memory()` is
    /// pinned to a single connection. This one uses a file-backed pool so real
    /// connections contend, which is the only way to catch a claim that is not
    /// actually atomic.
    #[tokio::test]
    async fn concurrent_workers_never_claim_the_same_job_twice() {
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.db");
        let store = Store::connect(&crate::config::StoreConfig {
            path: path.to_string_lossy().to_string(),
        })
        .await
        .unwrap();

        const JOBS: usize = 200;
        const WORKERS: usize = 8;
        for i in 0..JOBS {
            store
                .enqueue(Stage::Embed, "artifact", &format!("c-{i}"))
                .await
                .unwrap();
        }

        let claimed: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for _ in 0..WORKERS {
            let store = store.clone();
            let claimed = Arc::clone(&claimed);
            handles.push(tokio::spawn(async move {
                while let Some(job) = store.claim_job().await.unwrap() {
                    claimed.lock().unwrap().push(job.id);
                    store.complete_job(job.id).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let ids = claimed.lock().unwrap().clone();
        let unique: HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "a job was claimed more than once: {} claims for {} distinct jobs",
            ids.len(),
            unique.len()
        );
        assert_eq!(ids.len(), JOBS, "some jobs were never claimed");
    }

    #[tokio::test]
    async fn failure_reschedules_with_backoff_then_gives_up() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();

        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, j.attempts, "endpoint down").await.unwrap();
        // Backed off: not immediately claimable.
        assert!(s.claim_job().await.unwrap().is_none());

        // Burn the remaining attempts by moving run_after into the past.
        for _ in 0..MAX_ATTEMPTS {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&s.pool)
                .await
                .unwrap();
            if let Some(j) = s.claim_job().await.unwrap() {
                s.fail_job(j.id, j.attempts, "still down").await.unwrap();
            }
        }
        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.pool)
            .await
            .unwrap();
        assert!(
            s.claim_job().await.unwrap().is_none(),
            "exhausted job must stay failed"
        );

        let failed = s.failed_jobs(10).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].last_error.as_deref(), Some("still down"));
    }

    #[tokio::test]
    async fn stuck_running_jobs_are_reclaimed_after_a_crash() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        // Simulate the process dying mid-job: row left 'running'.
        sqlx::query("UPDATE jobs SET claimed_at = ? WHERE id = ?")
            .bind(crate::store::now() - 3600)
            .bind(j.id)
            .execute(&s.pool)
            .await
            .unwrap();

        assert_eq!(s.reclaim_stuck(600).await.unwrap(), 1);
        assert!(
            s.claim_job().await.unwrap().is_some(),
            "reclaimed job must be runnable again"
        );
    }

    #[tokio::test]
    async fn oldest_pending_age_is_a_waiting_time_not_a_timestamp() {
        let s = Store::memory().await.unwrap();
        assert_eq!(s.oldest_pending_age().await.unwrap(), None);

        s.enqueue(Stage::Synthesize, "corpus", "src-1")
            .await
            .unwrap();
        let age = s.oldest_pending_age().await.unwrap().unwrap();
        assert!(age < 5, "a just-enqueued job reported an age of {age}s");

        // A job enqueued an hour ago should read as roughly an hour.
        sqlx::query("UPDATE jobs SET created_at = ?")
            .bind(crate::store::now() - 3600)
            .execute(&s.pool)
            .await
            .unwrap();
        let age = s.oldest_pending_age().await.unwrap().unwrap();
        assert!((3595..=3605).contains(&age), "got {age}");
    }

    #[tokio::test]
    async fn requeue_revives_a_failed_job() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();
        let j = s.claim_job().await.unwrap().unwrap();
        sqlx::query("UPDATE jobs SET state='failed' WHERE id = ?")
            .bind(j.id)
            .execute(&s.pool)
            .await
            .unwrap();

        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();
        let again = s.claim_job().await.unwrap().unwrap();
        assert_eq!(again.attempts, 1, "requeue must reset the attempt counter");
    }
}
