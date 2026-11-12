//! Idle-pass evaluations, one row per pass, kept beside the settings they ran under.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// The same type as a generation's params, so the two records stay comparable.
pub type RunParams = super::generations::GenerationParams;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiffRow {
    pub query: String,
    /// `None` means the answer was not in the first ten at all.
    pub base: Option<usize>,
    pub new: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct NewEvalRun {
    pub judged_count: i64,
    pub pairs_used: i64,
    pub pairs_skipped: i64,
    pub base: RunParams,
    pub base_recall: f64,
    pub base_mrr: f64,
    pub best: RunParams,
    pub best_recall: f64,
    pub best_mrr: f64,
    pub diff: Vec<DiffRow>,
}

#[derive(Debug, Clone)]
pub struct EvalRun {
    pub id: String,
    pub created_at: i64,
    pub judged_count: i64,
    pub pairs_used: i64,
    pub pairs_skipped: i64,
    pub base_params: RunParams,
    pub base_recall: f64,
    pub base_mrr: f64,
    pub best_params: RunParams,
    pub best_recall: f64,
    pub best_mrr: f64,
    pub diff: Vec<DiffRow>,
}

impl Store {
    pub async fn record_eval_run(&self, run: &NewEvalRun) -> Result<String> {
        let id = new_id();
        sqlx::query!(
            "INSERT INTO eval_runs
               (id, created_at, judged_count, pairs_used, pairs_skipped,
                base_params, base_recall, base_mrr,
                best_params, best_recall, best_mrr,
                diff)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &id,
            now(),
            run.judged_count,
            run.pairs_used,
            run.pairs_skipped,
            json(&run.base)?,
            run.base_recall,
            run.base_mrr,
            json(&run.best)?,
            run.best_recall,
            run.best_mrr,
            json(&run.diff)?
        )
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn latest_eval_run(&self) -> Result<Option<EvalRun>> {
        let row = sqlx::query("SELECT * FROM eval_runs ORDER BY created_at DESC, id DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(hydrate).transpose()
    }
}

/// A row this binary cannot parse is an error: defaulting its settings would
/// measure against a baseline nobody ran.
fn parse<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|e| Error::Store(format!("eval_runs: {e}")))
}

fn json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Store(format!("eval_runs: {e}")))
}

fn hydrate(row: sqlx::sqlite::SqliteRow) -> Result<EvalRun> {
    Ok(EvalRun {
        id: row.get("id"),
        created_at: row.get("created_at"),
        judged_count: row.get("judged_count"),
        pairs_used: row.get("pairs_used"),
        pairs_skipped: row.get("pairs_skipped"),
        base_params: parse(row.get("base_params"))?,
        base_recall: row.get("base_recall"),
        base_mrr: row.get("base_mrr"),
        best_params: parse(row.get("best_params"))?,
        best_recall: row.get("best_recall"),
        best_mrr: row.get("best_mrr"),
        diff: parse(row.get("diff"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_written_before_the_retrieval_knobs_still_reads() {
        let p: RunParams = parse(r#"{"recency_weight":0.05,"per_source_cap":3}"#).unwrap();
        assert_eq!(p.candidate_multiplier, 3);
        assert_eq!(p.recency_half_life_days, 180);
    }

    fn sample(moved: bool) -> NewEvalRun {
        let base = RunParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        let best = if moved {
            RunParams {
                recency_weight: 0.1,
                per_source_cap: None,
                ..Default::default()
            }
        } else {
            base
        };
        NewEvalRun {
            judged_count: 50,
            pairs_used: 12,
            pairs_skipped: 1,
            base,
            base_recall: 0.70,
            base_mrr: 0.50,
            best,
            best_recall: if moved { 0.80 } else { 0.70 },
            best_mrr: if moved { 0.60 } else { 0.50 },
            diff: vec![DiffRow {
                query: "the image will not mount".into(),
                base: None,
                new: Some(2),
            }],
        }
    }

    #[tokio::test]
    async fn the_latest_run_is_the_last_word() {
        let store = Store::memory().await.unwrap();
        assert!(store.latest_eval_run().await.unwrap().is_none());
        let old = store.record_eval_run(&sample(true)).await.unwrap();
        let new = store.record_eval_run(&sample(false)).await.unwrap();
        let latest = store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(latest.id, new);
        assert_ne!(latest.id, old);
        assert_eq!(latest.base_params, latest.best_params);
        assert_eq!(latest.pairs_skipped, 1);
    }

    #[tokio::test]
    async fn a_run_reads_back_with_the_settings_and_the_pairs_that_moved() {
        let store = Store::memory().await.unwrap();
        store.record_eval_run(&sample(true)).await.unwrap();
        let run = store.latest_eval_run().await.unwrap().unwrap();
        assert_eq!(run.base_params.per_source_cap, Some(3));
        assert_eq!(run.best_params.per_source_cap, None);
        assert!(run.best_mrr > run.base_mrr);
        assert_eq!(run.diff.len(), 1);
        assert_eq!(run.diff[0].new, Some(2));
    }
}
