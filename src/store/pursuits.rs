//! What happens after a result list renders, and the pursuits it is grouped
//! into at analysis.
//!
//! An interaction carries no pursuit id: the clustering decides which pursuit
//! a search event belongs to, and an interaction is joined to that event by
//! time and scope when the sweep runs. Re-clustering never rewrites these rows.

use super::Store;
use crate::error::{Error, Result};
use sqlx::Row;

/// The identity of a pursuit: when the sitting opened and what was asked in
/// it. Queries are normalised and sorted first, so the same sitting re-read
/// after a crash hashes the same however the clusterer happened to order it.
fn pursuit_id(opened_at: i64, queries: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut keys: Vec<String> = queries
        .iter()
        .map(|q| format!("{}\n", super::links::normalize_query(q)))
        .collect();
    keys.sort();
    keys.dedup();
    hex::encode(Sha256::digest(
        format!("{opened_at}\n{}", keys.concat()).as_bytes(),
    ))
}

/// One thing done with a result: opened, or reached from another artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct Interaction {
    pub id: i64,
    pub artifact_id: String,
    /// `opened` | `pivoted`
    pub kind: String,
    /// The artifact this was reached from, for `pivoted`.
    pub via: Option<String>,
    /// Seconds, for `dwell`.
    pub detail: Option<String>,
    pub scope: Option<String>,
    pub at: i64,
}

/// A coherent thing that was wanted, and what came of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pursuit {
    pub id: String,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    /// open | satisfied | unsatisfied | generated | dismissed
    pub state: String,
    pub reason: Option<String>,
    pub queries: Vec<String>,
    pub sources: Vec<String>,
    pub artifact_id: Option<String>,
}

/// Shown against clicked, for one rung of the ladder.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OfferRate {
    pub rung: String,
    pub shown: i64,
    pub opened: i64,
}

fn row_to_pursuit(r: &sqlx::sqlite::SqliteRow) -> Pursuit {
    Pursuit {
        id: r.get("id"),
        opened_at: r.get("opened_at"),
        closed_at: r.get("closed_at"),
        state: r.get("state"),
        reason: r.get("reason"),
        queries: serde_json::from_str(&r.get::<String, _>("queries")).unwrap_or_default(),
        sources: serde_json::from_str(&r.get::<String, _>("sources")).unwrap_or_default(),
        artifact_id: r.get("artifact_id"),
    }
}

impl Store {
    /// One interaction, stamped `at`. `kind` is `opened` or `pivoted`.
    pub async fn record_interaction(
        &self,
        artifact_id: &str,
        kind: &str,
        via: Option<&str>,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO interaction_events (artifact_id, kind, via, scope, at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(artifact_id)
        .bind(kind)
        .bind(via)
        .bind(scope)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How long an artifact stayed open, in seconds. The weakest signal there
    /// is — long means useful or means confusing, a tab left open means
    /// engaged or means lunch — so it is a tiebreak in the sweep and never
    /// decisive.
    pub async fn record_dwell(
        &self,
        artifact_id: &str,
        secs: i64,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO interaction_events (artifact_id, kind, detail, scope, at)
             VALUES (?, 'dwell', ?, ?, ?)",
        )
        .bind(artifact_id)
        .bind(secs.to_string())
        .bind(scope)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// What this base offered, and whether it was taken.
    ///
    /// `kind` is `recommended_shown` or `recommended_open`. Both live in
    /// `interaction_events` because both are things that happened after a page
    /// rendered — but neither counts as an ordinary open: the context sweep
    /// reads `recommended_open` at `recommend.self_weight` and ignores
    /// `recommended_shown` entirely, and the pursuit sweep skips the latter too.
    ///
    /// `detail` carries the rung and the winning cluster as JSON, which is what
    /// makes the Ops hit rate a breakdown rather than one number.
    pub async fn record_recommendation(
        &self,
        artifact_id: &str,
        kind: &str,
        detail: &str,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO interaction_events (artifact_id, kind, detail, scope, at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(artifact_id)
        .bind(kind)
        .bind(detail)
        .bind(scope)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// What was offered and what was taken, by rung, since `since`.
    ///
    /// The only number that can later settle whether the block weights are
    /// right. They are chosen, not measured, and fitting them before this data
    /// exists would be guessing with extra steps — so this is the instrument,
    /// and it goes on Ops, which is where mechanisms whose effect nobody can
    /// otherwise see belong.
    pub async fn offer_rates(&self, since: i64) -> Result<Vec<OfferRate>> {
        let rows = sqlx::query(
            "SELECT json_extract(detail, '$.rung') AS rung,
                    SUM(kind = 'recommended_shown') AS shown,
                    SUM(kind = 'recommended_open')  AS opened
               FROM interaction_events
              WHERE at >= ? AND kind IN ('recommended_shown', 'recommended_open')
              GROUP BY rung
              ORDER BY shown DESC, rung",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| OfferRate {
                rung: r
                    .get::<Option<String>, _>("rung")
                    .unwrap_or_else(|| "unknown".into()),
                shown: r.get("shown"),
                opened: r.get("opened"),
            })
            .collect())
    }

    /// Interactions with `from < at <= to`, oldest first.
    pub async fn interactions_between(&self, from: i64, to: i64) -> Result<Vec<Interaction>> {
        let rows = sqlx::query(
            "SELECT id, artifact_id, kind, via, detail, scope, at FROM interaction_events
              WHERE at > ? AND at <= ? ORDER BY at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Interaction {
                id: r.get("id"),
                artifact_id: r.get("artifact_id"),
                kind: r.get("kind"),
                via: r.get("via"),
                detail: r.get("detail"),
                scope: r.get("scope"),
                at: r.get("at"),
            })
            .collect())
    }

    /// Drop interactions past keeping.
    ///
    /// The table had no sweep at all, only the manual `purge_pursuits`, while
    /// the situations it is read beside got `context::RETAIN_DAYS` from the
    /// start. That was survivable while a row meant an open; it stopped being
    /// so when the offer began writing a `recommended_shown` per page view, so
    /// the table grew with browsing rather than with use — and the context
    /// sweep reads the whole window into memory every six hours.
    ///
    /// The same window as the situations, and for the same reason: the sweep
    /// pairs the two, and an interaction kept past the situation it happened in
    /// profiles nothing. Nothing else reads further back — `offer_rates` asks
    /// for a month, and the pursuit sweep works from a cursor.
    pub async fn expire_interactions(&self, retain_days: i64) -> Result<u64> {
        let cutoff = crate::store::now() - retain_days * 86_400;
        Ok(sqlx::query("DELETE FROM interaction_events WHERE at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// A new pursuit, `open`. Returns its id.
    ///
    /// Idempotent by identity rather than by insertion order: the id is the
    /// cluster itself — when it opened and what was asked — so the same
    /// sitting swept twice is the same row twice. The sweep needs that. It
    /// writes a pursuit per cluster and advances its cursor once at the end,
    /// so a failure in the middle of the loop leaves rows written under a
    /// cursor that never moved, and the retry re-reads the same events and
    /// re-clusters them identically. Without this the operator would find the
    /// sitting listed once per crash on Ops, each copy able to arm its own
    /// generation.
    /// `query_vec` is the leading clustered query's vector and the model that
    /// produced it, carried forward because a pursuit that closes unsatisfied
    /// is a gap and a gap is a question plus the vector it was found by. The
    /// sweep is holding both already; re-embedding the words later would be a
    /// call spent on a vector that has been computed once.
    pub async fn insert_pursuit(
        &self,
        opened_at: i64,
        queries: &[String],
        sources: &[String],
        query_vec: Option<(&[f32], &str)>,
    ) -> Result<String> {
        let id = pursuit_id(opened_at, queries);
        let (blob, dim, model) = match query_vec {
            Some((v, m)) if !v.is_empty() => (
                crate::store::feedback::vec_to_blob(v),
                v.len() as i64,
                Some(m.to_string()),
            ),
            _ => (Vec::new(), 0, None),
        };
        sqlx::query(
            "INSERT OR IGNORE INTO pursuits
               (id, opened_at, state, queries, sources, query_vec, vec_dim, embed_model)
             VALUES (?, ?, 'open', ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(opened_at)
        .bind(serde_json::to_string(queries).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(sources).unwrap_or_else(|_| "[]".into()))
        .bind(blob)
        .bind(dim)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Close a pursuit with a state and a one-line reason.
    pub async fn close_pursuit(&self, id: &str, state: &str, reason: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE pursuits SET state = ?, reason = ?, closed_at = ? WHERE id = ?")
            .bind(state)
            .bind(reason)
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The generation landed: name the artifact, state `generated`.
    pub async fn set_pursuit_artifact(&self, id: &str, artifact_id: &str, at: i64) -> Result<()> {
        sqlx::query(
            "UPDATE pursuits SET artifact_id = ?, state = 'generated', closed_at = ? WHERE id = ?",
        )
        .bind(artifact_id)
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pursuit(&self, id: &str) -> Result<Pursuit> {
        let row = sqlx::query("SELECT * FROM pursuits WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_pursuit(&row))
    }

    /// Newest first. What Ops lists.
    pub async fn recent_pursuits(&self, limit: i64) -> Result<Vec<Pursuit>> {
        let rows = sqlx::query("SELECT * FROM pursuits ORDER BY opened_at DESC LIMIT ?")
            .bind(limit.max(0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_pursuit).collect())
    }

    /// How many pursuits are in one state, with no page over it.
    ///
    /// `recent_pursuits` answers a page for a list to be drawn from. Counting
    /// that page reported the page size as a total the moment a base held more
    /// pursuits than the page held rows.
    pub async fn count_pursuits(&self, state: &str) -> Result<i64> {
        use sqlx::Row;
        Ok(
            sqlx::query("SELECT COUNT(*) AS n FROM pursuits WHERE state = ?")
                .bind(state)
                .fetch_one(&self.pool)
                .await?
                .get("n"),
        )
    }

    /// Forget every pursuit and every interaction. Rows dropped.
    pub async fn purge_pursuits(&self) -> Result<u64> {
        let a = sqlx::query("DELETE FROM interaction_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        let b = sqlx::query("DELETE FROM pursuits")
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(a + b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;

    #[tokio::test]
    async fn interactions_are_recorded_and_read_back_in_range() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let a = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        s.record_interaction(&a, "opened", None, Some("u1"), 10)
            .await
            .unwrap();
        s.record_interaction(&a, "pivoted", Some("other"), Some("u1"), 20)
            .await
            .unwrap();
        let got = s.interactions_between(0, 15).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "opened");
        let got = s.interactions_between(0, 100).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].via.as_deref(), Some("other"));
        assert_eq!(got[1].scope.as_deref(), Some("u1"));
        s.record_dwell(&a, 45, Some("u1"), 30).await.unwrap();
        let got = s.interactions_between(0, 100).await.unwrap();
        assert_eq!(got[2].kind, "dwell");
        assert_eq!(got[2].detail.as_deref(), Some("45"));
    }

    #[tokio::test]
    async fn interactions_past_the_window_are_dropped_and_the_rest_are_kept() {
        // The table had no sweep of any kind. That was survivable while a row
        // meant somebody opened something; it stopped being so when the offer
        // began writing a `recommended_shown` per search-page view, because the
        // context sweep reads the whole window into memory every six hours and
        // the window had no end.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let a = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone();
        let now = crate::store::now();
        let retain = crate::store::context::RETAIN_DAYS;
        let old = now - (retain + 1) * 86_400;
        let recent = now - 86_400;
        s.record_interaction(&a, "opened", None, Some("u1"), old)
            .await
            .unwrap();
        s.record_recommendation(&a, "recommended_shown", "{}", Some("u1"), old)
            .await
            .unwrap();
        s.record_interaction(&a, "opened", None, Some("u1"), recent)
            .await
            .unwrap();

        assert_eq!(s.expire_interactions(retain).await.unwrap(), 2);
        let got = s.interactions_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].at, recent);
    }

    #[tokio::test]
    async fn a_pursuit_round_trips_and_closes() {
        let s = Store::memory().await.unwrap();
        let id = s
            .insert_pursuit(
                100,
                &["how to mount".into()],
                &["a1".into(), "a2".into()],
                None,
            )
            .await
            .unwrap();
        let p = s.get_pursuit(&id).await.unwrap();
        assert_eq!(p.state, "open");
        assert_eq!(p.queries, vec!["how to mount".to_string()]);
        assert_eq!(p.sources, vec!["a1".to_string(), "a2".to_string()]);
        s.close_pursuit(&id, "satisfied", "answered", 200)
            .await
            .unwrap();
        let p = s.get_pursuit(&id).await.unwrap();
        assert_eq!((p.state.as_str(), p.closed_at), ("satisfied", Some(200)));
        assert_eq!(p.reason.as_deref(), Some("answered"));
        assert_eq!(s.recent_pursuits(10).await.unwrap().len(), 1);
        assert_eq!(s.purge_pursuits().await.unwrap(), 1);
        assert!(s.recent_pursuits(10).await.unwrap().is_empty());
    }
}
