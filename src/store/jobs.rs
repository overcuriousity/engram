use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Where a job's behaviour changes, not where it is abandoned.
///
/// Past this many attempts a stage may switch tactics — splitting a batch
/// embed into one job per artifact, recording which segments the synthesizer
/// refused — but the work stays queued either way, at the backoff's ceiling.
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

/// Work waiting out a backoff. What replaced the failed list: there is no
/// terminal state to report, only a next attempt to name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetryingJob {
    pub stage: String,
    pub target_id: String,
    pub attempts: i64,
    pub next_attempt_secs: i64,
    pub last_error: Option<String>,
}

/// 2s, 4s, 8s, 16s, 32s ... doubling to a six-hour ceiling, and never stopping.
///
/// The ceiling used to be five minutes, which suited a caller that gave up
/// after five attempts — one minute of patience in total. An inference endpoint
/// that loads a model on demand takes ten, so the whole budget was spent before
/// the endpoint had finished starting, and the work was lost until a person
/// noticed and pressed a button. Six hours is short enough that a base heals
/// the same day and long enough that text the model will never accept costs
/// four calls a day rather than a thousand.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.clamp(1, 16) as u32;
    2i64.saturating_pow(exp).min(21_600)
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

    /// Queue work that has already been tried, so the next attempt waits.
    ///
    /// `enqueue` resets `attempts` to zero, which is right for a reprocess a
    /// person asked for and wrong for a stage re-arming itself: a synthesize
    /// job that keeps failing would come straight back with a two-second
    /// delay and hammer an endpoint that is down. This keeps the attempt count
    /// climbing so the backoff means something.
    pub async fn enqueue_after(
        &self,
        stage: Stage,
        target_kind: &str,
        target_id: &str,
        attempts: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO jobs (stage, target_kind, target_id, state, attempts, run_after, created_at)
             VALUES (?, ?, ?, 'pending', ?, ?, ?)
             ON CONFLICT(stage, target_id) DO UPDATE SET
               state = 'pending', attempts = excluded.attempts,
               run_after = excluded.run_after, claimed_at = NULL",
        )
        .bind(stage.as_str())
        .bind(target_kind)
        .bind(target_id)
        .bind(attempts)
        .bind(now() + backoff_secs(attempts))
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

    /// Put a job back in the queue with a delay.
    ///
    /// There is no terminal state. `attempts` past `MAX_ATTEMPTS` only means
    /// the delay has reached its ceiling: a base that cannot reach its
    /// endpoint should cost nothing and heal when the endpoint returns, and
    /// the previous behaviour — mark it failed, close it, wait for a human —
    /// turned a ten-minute outage into permanently missing knowledge.
    pub async fn fail_job(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        sqlx::query(
            "UPDATE jobs SET state = 'pending', run_after = ?, last_error = ?, claimed_at = NULL WHERE id = ?",
        )
        .bind(now() + backoff_secs(attempts))
        .bind(err)
        .bind(id)
        .execute(&self.pool)
        .await?;
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

    /// Jobs waiting on a backoff, soonest first.
    ///
    /// `attempts > 0` is what separates work that has hit something from work
    /// that is merely queued: a fresh job has `run_after` in the past and does
    /// not belong on a page about trouble.
    pub async fn retrying_jobs(&self, limit: i64) -> Result<Vec<RetryingJob>> {
        let rows = sqlx::query(
            "SELECT stage, target_id, attempts, last_error, run_after FROM jobs
              WHERE state = 'pending' AND attempts > 0 AND run_after > ?
              ORDER BY run_after LIMIT ?",
        )
        .bind(now())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let at = now();
        Ok(rows
            .iter()
            .map(|r| RetryingJob {
                stage: r.get("stage"),
                target_id: r.get("target_id"),
                attempts: r.get("attempts"),
                next_attempt_secs: (r.get::<i64, _>("run_after") - at).max(0),
                last_error: r.get("last_error"),
            })
            .collect())
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
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(100), 21_600, "must cap, not grow unbounded");
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
    async fn failure_reschedules_with_backoff_and_keeps_the_work() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "c-1").await.unwrap();

        let j = s.claim_job().await.unwrap().unwrap();
        s.fail_job(j.id, j.attempts, "endpoint down").await.unwrap();
        // Backed off: not immediately claimable.
        assert!(s.claim_job().await.unwrap().is_none());

        // Well past the old give-up point.
        for _ in 0..MAX_ATTEMPTS + 3 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&s.pool)
                .await
                .unwrap();
            let j = s
                .claim_job()
                .await
                .unwrap()
                .expect("the job must still be there to try again");
            s.fail_job(j.id, j.attempts, "still down").await.unwrap();
        }

        sqlx::query("UPDATE jobs SET run_after = 0")
            .execute(&s.pool)
            .await
            .unwrap();
        let again = s.claim_job().await.unwrap();
        assert!(
            again.is_some(),
            "the work was abandoned; an endpoint that comes back would never be noticed"
        );
        assert!(s.failed_jobs(10).await.unwrap().is_empty());
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

    #[test]
    fn backoff_climbs_to_hours_and_stops_there() {
        // An endpoint that is down stays down for minutes; one loading a model
        // on demand takes ten. The old ceiling of five minutes went with a
        // caller that gave up after five attempts — one minute of patience in
        // total, spent before the endpoint had finished starting.
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(20), 21_600);
        assert_eq!(backoff_secs(1_000), 21_600);
    }

    #[tokio::test]
    async fn a_job_out_of_attempts_waits_rather_than_failing() {
        let s = Store::memory().await.unwrap();
        s.enqueue(Stage::Embed, "artifact", "a1").await.unwrap();
        let job = s.claim_job().await.unwrap().unwrap();
        s.fail_job(job.id, MAX_ATTEMPTS + 10, "endpoint down")
            .await
            .unwrap();
        assert!(
            s.failed_jobs(10).await.unwrap().is_empty(),
            "a job was abandoned; nothing would ever pick it up again"
        );
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = ?")
            .bind(job.id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(state, "pending");
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
