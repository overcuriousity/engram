//! The journal a person reads in the morning: one row per sleep.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SleepRun {
    pub id: String,
    pub started: i64,
    pub ended: i64,
    /// `finished` | `activity` | `suspended` | `no_evidence` | `budget`
    pub stopped: String,
    pub generation_id: String,
    pub integrated: i64,
    pub novel: i64,
    pub known: i64,
    pub conflicts: i64,
    pub rehearsed: i64,
    pub found: i64,
    pub adopted: Option<String>,
    pub reverted: Option<String>,
    pub refused: Option<String>,
    pub undone: i64,
    pub restored: i64,
    pub interference: i64,
    pub condensed: i64,
    pub budget_used: i64,
    pub budget: i64,
    /// JSON: `{"actions":[…], "pairs":[…]}`.
    pub detail: String,
}

impl Store {
    pub async fn record_sleep_run(&self, r: &SleepRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO sleep_runs
               (id, started, ended, stopped, generation_id, integrated, novel, known, conflicts,
                rehearsed, found, adopted, reverted, refused, undone, restored, interference,
                condensed, budget_used, budget, detail)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&r.id)
        .bind(r.started)
        .bind(if r.ended == 0 { now() } else { r.ended })
        .bind(&r.stopped)
        .bind(&r.generation_id)
        .bind(r.integrated)
        .bind(r.novel)
        .bind(r.known)
        .bind(r.conflicts)
        .bind(r.rehearsed)
        .bind(r.found)
        .bind(&r.adopted)
        .bind(&r.reverted)
        .bind(&r.refused)
        .bind(r.undone)
        .bind(r.restored)
        .bind(r.interference)
        .bind(r.condensed)
        .bind(r.budget_used)
        .bind(r.budget)
        .bind(if r.detail.is_empty() {
            "{}"
        } else {
            r.detail.as_str()
        })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Newest first.
    pub async fn sleep_runs(&self, limit: usize) -> Result<Vec<SleepRun>> {
        let rows = sqlx::query("SELECT * FROM sleep_runs ORDER BY started DESC, id DESC LIMIT ?")
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| SleepRun {
                id: r.get("id"),
                started: r.get("started"),
                ended: r.get("ended"),
                stopped: r.get("stopped"),
                generation_id: r.get("generation_id"),
                integrated: r.get("integrated"),
                novel: r.get("novel"),
                known: r.get("known"),
                conflicts: r.get("conflicts"),
                rehearsed: r.get("rehearsed"),
                found: r.get("found"),
                adopted: r.get("adopted"),
                reverted: r.get("reverted"),
                refused: r.get("refused"),
                undone: r.get("undone"),
                restored: r.get("restored"),
                interference: r.get("interference"),
                condensed: r.get("condensed"),
                budget_used: r.get("budget_used"),
                budget: r.get("budget"),
                detail: r.get("detail"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_sleep_is_written_once_and_read_newest_first() {
        let store = Store::memory().await.unwrap();
        for i in 0..2 {
            store
                .record_sleep_run(&SleepRun {
                    id: crate::store::new_id(),
                    started: i,
                    ended: i + 1,
                    stopped: "finished".into(),
                    generation_id: "g".into(),
                    integrated: i,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let runs = store.sleep_runs(10).await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].integrated, 1, "newest first");
        assert_eq!(runs[0].detail, "{}");
    }
}
