//! What the memory did while nobody was looking.
//!
//! One row per completed run of a periodic unit. There is no "night" to group
//! them by: units that reschedule themselves on their own periods do not line
//! up into one cycle, and inventing a cycle identity to group them by would be
//! inventing it. What Ops shows is the last day, and under it the history —
//! which is the thing a single overwritten summary could never give, namely
//! whether a sweep started going wrong yesterday or has been going wrong for a
//! week.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// How many runs are kept. Trimmed by the repair pass, which is behind no
/// setting: a trim that rode on the retention unit stopped whenever capture was
/// switched off, and the sweeps that kept running kept writing rows.
/// Housekeeping about housekeeping does not get a policy key.
pub const MAX_RUNS: i64 = 2000;

/// One recorded run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepRun {
    pub stage: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// `ok` or `failed`.
    pub outcome: String,
    /// The counts the sweep returned, as JSON.
    pub detail: String,
}

impl Store {
    /// Record what one run of a periodic unit did.
    ///
    /// A failed run is recorded like any other. It is exactly the run an
    /// operator needs to see, and a history that only kept the successes would
    /// show a sweep going quiet with nothing anywhere saying why.
    pub async fn record_sweep_run(
        &self,
        stage: &str,
        started_at: i64,
        outcome: &str,
        detail: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO sweep_runs (stage, started_at, ended_at, outcome, detail)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(stage)
        .bind(started_at)
        .bind(now())
        .bind(outcome)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every run since `since`, newest first.
    pub async fn sweep_runs_since(&self, since: i64, limit: i64) -> Result<Vec<SweepRun>> {
        let rows = sqlx::query(
            "SELECT stage, started_at, ended_at, outcome, detail FROM sweep_runs
              WHERE started_at >= ? ORDER BY started_at DESC LIMIT ?",
        )
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(read_run).collect())
    }

    /// The history: the most recent runs, newest first, whenever they were.
    pub async fn sweep_history(&self, limit: i64) -> Result<Vec<SweepRun>> {
        let rows = sqlx::query(
            "SELECT stage, started_at, ended_at, outcome, detail FROM sweep_runs
              ORDER BY started_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(read_run).collect())
    }

    /// Keep the newest `MAX_RUNS` and forget the rest.
    pub async fn trim_sweep_runs(&self) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM sweep_runs WHERE id NOT IN (
               SELECT id FROM sweep_runs ORDER BY started_at DESC, id DESC LIMIT ?
             )",
        )
        .bind(MAX_RUNS)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

fn read_run(r: &sqlx::sqlite::SqliteRow) -> SweepRun {
    SweepRun {
        stage: r.get("stage"),
        started_at: r.get("started_at"),
        ended_at: r.get("ended_at"),
        outcome: r.get("outcome"),
        detail: r.get("detail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_run_is_recorded_like_any_other() {
        let s = Store::memory().await.unwrap();
        s.record_sweep_run("associate", now(), "ok", r#"{"events":3}"#)
            .await
            .unwrap();
        s.record_sweep_run("consolidate", now(), "failed", "{}")
            .await
            .unwrap();

        let runs = s.sweep_history(10).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert!(
            runs.iter().any(|r| r.outcome == "failed"),
            "a history that keeps only the successes shows a sweep going quiet \
             with nothing saying why"
        );
    }

    #[tokio::test]
    async fn the_last_day_leaves_out_what_is_older() {
        let s = Store::memory().await.unwrap();
        s.record_sweep_run("associate", now() - 2 * 86_400, "ok", "{}")
            .await
            .unwrap();
        s.record_sweep_run("associate", now(), "ok", "{}")
            .await
            .unwrap();

        assert_eq!(
            s.sweep_runs_since(now() - 86_400, 100).await.unwrap().len(),
            1
        );
        assert_eq!(s.sweep_history(100).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_trim_keeps_the_newest() {
        let s = Store::memory().await.unwrap();
        for i in 0..(MAX_RUNS + 5) {
            s.record_sweep_run("retention", now() - (MAX_RUNS + 5 - i), "ok", "{}")
                .await
                .unwrap();
        }
        assert_eq!(s.trim_sweep_runs().await.unwrap(), 5);
        let kept = s.sweep_history(MAX_RUNS + 10).await.unwrap();
        assert_eq!(kept.len() as i64, MAX_RUNS);
    }
}
