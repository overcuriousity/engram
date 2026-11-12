//! One row per completed run of a periodic unit. Units keep their own periods: no cycle id.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// Trimmed by the repair pass, which no setting switches off: the sweeps keep writing regardless.
pub const MAX_RUNS: i64 = 2000;

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
    /// Failed runs are recorded too: they are the ones an operator needs to see.
    pub async fn record_sweep_run(
        &self,
        stage: &str,
        started_at: i64,
        outcome: &str,
        detail: &str,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO sweep_runs (stage, started_at, ended_at, outcome, detail)
             VALUES (?, ?, ?, ?, ?)",
            stage,
            started_at,
            now(),
            outcome,
            detail
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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

    pub async fn last_sweep_run(&self, stage: &str) -> Result<Option<SweepRun>> {
        let row = sqlx::query(
            "SELECT stage, started_at, ended_at, outcome, detail FROM sweep_runs
              WHERE stage = ? ORDER BY started_at DESC LIMIT 1",
        )
        .bind(stage)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(read_run))
    }

    pub async fn trim_sweep_runs(&self) -> Result<u64> {
        let res = sqlx::query!(
            "DELETE FROM sweep_runs WHERE id NOT IN (
               SELECT id FROM sweep_runs ORDER BY started_at DESC, id DESC LIMIT ?
             )",
            MAX_RUNS
        )
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
