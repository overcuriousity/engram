//! What a tuning sweep found, kept.
//!
//! The judge page's recall and MRR are read from the ranks the searches
//! actually gave, which is the measurement of the ranking that produced them.
//! A sweep asks the other question — what *these* pairs would score under
//! other settings — and the answer is only worth anything beside the settings
//! that produced it. Hence a row per sweep rather than a number on a page.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// The runtime-tunable knobs, as stored. Mirrors
/// `core::ranking::RankingParams`; separate because what is written to a
/// database outlives the shape a running program happens to hold it in.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunParams {
    pub recency_weight: f32,
    pub per_source_cap: Option<usize>,
    /// Absent in rows written before the retrieval knobs existed, which ran
    /// under the shipped value.
    #[serde(default = "crate::config::default_candidate_multiplier")]
    pub candidate_multiplier: usize,
    #[serde(default = "crate::config::default_recency_half_life_days")]
    pub recency_half_life_days: u32,
    #[serde(default = "crate::config::default_prime_lift")]
    pub prime_lift: usize,
    #[serde(default = "crate::config::default_spread_max")]
    pub spread_max: usize,
    #[serde(default = "crate::config::default_rerank_knob")]
    pub rerank: bool,
}

impl Default for RunParams {
    fn default() -> Self {
        crate::core::ranking::RankingParams::default().into()
    }
}

impl From<crate::core::ranking::RankingParams> for RunParams {
    fn from(p: crate::core::ranking::RankingParams) -> Self {
        Self {
            recency_weight: p.recency_weight,
            per_source_cap: p.per_source_cap,
            candidate_multiplier: p.candidate_multiplier,
            recency_half_life_days: p.recency_half_life_days,
            prime_lift: p.prime_lift,
            spread_max: p.spread_max,
            rerank: p.rerank,
        }
    }
}

impl From<RunParams> for crate::core::ranking::RankingParams {
    fn from(p: RunParams) -> Self {
        Self {
            recency_weight: p.recency_weight,
            per_source_cap: p.per_source_cap,
            candidate_multiplier: p.candidate_multiplier,
            recency_half_life_days: p.recency_half_life_days,
            prime_lift: p.prime_lift,
            spread_max: p.spread_max,
            rerank: p.rerank,
        }
    }
}

/// One pair that moved, named by the leading characters of its own query.
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
    pub recommended: bool,
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
    pub recommended: bool,
    pub applied_at: Option<i64>,
}

impl Store {
    pub async fn record_eval_run(&self, run: &NewEvalRun) -> Result<String> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO eval_runs
               (id, created_at, judged_count, pairs_used, pairs_skipped,
                base_params, base_recall, base_mrr,
                best_params, best_recall, best_mrr,
                diff, recommended)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(now())
        .bind(run.judged_count)
        .bind(run.pairs_used)
        .bind(run.pairs_skipped)
        .bind(json(&run.base)?)
        .bind(run.base_recall)
        .bind(run.base_mrr)
        .bind(json(&run.best)?)
        .bind(run.best_recall)
        .bind(run.best_mrr)
        .bind(json(&run.diff)?)
        .bind(run.recommended as i64)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// The most recent sweep, recommended or not. What paces the next one.
    pub async fn latest_eval_run(&self) -> Result<Option<EvalRun>> {
        let row = sqlx::query("SELECT * FROM eval_runs ORDER BY created_at DESC, id DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(hydrate).transpose()
    }

    /// The recommendation waiting for an answer, if there is one.
    ///
    /// The latest run, and only if that run is itself an open recommendation.
    /// Selecting the newest *recommended* row instead left every older one
    /// alive behind it: a sweep recommends X, a later sweep over more evidence
    /// recommends Y, the operator applies Y — and X, measured against a
    /// baseline that is no longer in force and already refused by the newer
    /// evidence, was offered again on the next render. A sweep is the last
    /// word on what these pairs say, including when what it says is nothing.
    pub async fn open_recommendation(&self) -> Result<Option<EvalRun>> {
        Ok(self
            .latest_eval_run()
            .await?
            .filter(|r| r.recommended && r.applied_at.is_none()))
    }

    pub async fn eval_run(&self, id: &str) -> Result<Option<EvalRun>> {
        let row = sqlx::query("SELECT * FROM eval_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(hydrate).transpose()
    }

    /// Stamp a run as applied. `false` if it was already, which is how a
    /// double-submitted apply is refused rather than counted twice.
    pub async fn mark_eval_run_applied(&self, id: &str) -> Result<bool> {
        let res =
            sqlx::query("UPDATE eval_runs SET applied_at = ? WHERE id = ? AND applied_at IS NULL")
                .bind(now())
                .bind(id)
                .execute(&self.pool)
                .await?;
        Ok(res.rows_affected() == 1)
    }

    /// What has actually been changed, newest first. The provenance a commit
    /// message used to carry.
    pub async fn applied_eval_runs(&self, limit: i64) -> Result<Vec<EvalRun>> {
        sqlx::query(
            "SELECT * FROM eval_runs WHERE applied_at IS NOT NULL
             ORDER BY applied_at DESC, id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(hydrate)
        .collect()
    }
}

/// A row this binary cannot read is a broken row, not an empty one: a sweep
/// silently rehydrated with default settings would recommend against a
/// baseline nobody ever ran.
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
        recommended: row.get::<i64, _>("recommended") == 1,
        applied_at: row.get("applied_at"),
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

    fn sample(recommended: bool) -> NewEvalRun {
        let base = RunParams {
            recency_weight: 0.05,
            per_source_cap: Some(3),
            ..Default::default()
        };
        let best = if recommended {
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
            best_recall: if recommended { 0.80 } else { 0.70 },
            best_mrr: if recommended { 0.60 } else { 0.50 },
            diff: vec![DiffRow {
                query: "the image will not mount".into(),
                base: None,
                new: Some(2),
            }],
            recommended,
        }
    }

    #[tokio::test]
    async fn a_recommendation_is_open_until_applied_and_applies_once() {
        let store = Store::memory().await.unwrap();
        let id = store.record_eval_run(&sample(true)).await.unwrap();
        assert_eq!(
            store.open_recommendation().await.unwrap().map(|r| r.id),
            Some(id.clone())
        );

        assert!(store.mark_eval_run_applied(&id).await.unwrap());
        assert!(
            store.open_recommendation().await.unwrap().is_none(),
            "an applied recommendation must stop being offered"
        );
        assert!(
            !store.mark_eval_run_applied(&id).await.unwrap(),
            "a replayed apply must be refused rather than stamped twice"
        );

        let applied = store.applied_eval_runs(10).await.unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].best_params.per_source_cap, None);
        assert_eq!(applied[0].diff.len(), 1);
    }

    #[tokio::test]
    async fn a_later_sweep_retires_the_recommendation_before_it() {
        // The stale one used to outlive the fresh one. A sweep recommends X,
        // more judgements arrive, a second sweep recommends Y, the operator
        // takes Y — and X came back on the next render, measured against a
        // baseline no longer in force and already refused by the newer
        // evidence. Applying it would have walked the ranking backwards.
        let store = Store::memory().await.unwrap();
        let old = store.record_eval_run(&sample(true)).await.unwrap();
        let new = store.record_eval_run(&sample(true)).await.unwrap();
        assert_eq!(
            store.open_recommendation().await.unwrap().map(|r| r.id),
            Some(new.clone())
        );

        assert!(store.mark_eval_run_applied(&new).await.unwrap());
        assert!(
            store.open_recommendation().await.unwrap().is_none(),
            "the sweep before the applied one was offered again"
        );
        // The row is not rewritten — it recorded what it found, and that stays
        // true. It is simply no longer the last word.
        let old = store.eval_run(&old).await.unwrap().unwrap();
        assert!(old.recommended && old.applied_at.is_none());

        // A sweep that found nothing is just as much the last word: it looked
        // at the same pairs, over more of them, and refused what the one before
        // it had offered.
        let quiet = Store::memory().await.unwrap();
        quiet.record_eval_run(&sample(true)).await.unwrap();
        quiet.record_eval_run(&sample(false)).await.unwrap();
        assert!(quiet.open_recommendation().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_quiet_sweep_is_recorded_but_never_recommended() {
        // The silence has to be explainable: without the row, a page can only
        // say nothing at all, which reads as "no sweep has ever run".
        let store = Store::memory().await.unwrap();
        store.record_eval_run(&sample(false)).await.unwrap();
        assert!(store.open_recommendation().await.unwrap().is_none());

        let latest = store.latest_eval_run().await.unwrap().unwrap();
        assert!(!latest.recommended);
        assert_eq!(latest.base_params, latest.best_params);
        assert_eq!(latest.pairs_skipped, 1);
    }
}
