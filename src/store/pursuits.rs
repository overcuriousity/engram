//! Interactions after a result list renders, and the pursuits they are grouped into.
//! Interactions carry no pursuit id; the sweep joins them by time and scope.

use super::Store;
use crate::error::{Error, Result};
use sqlx::Row;

/// Queries are normalised and sorted, so a re-read after a crash hashes the same.
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct OfferRate {
    pub rung: String,
    pub shown: i64,
    pub opened: i64,
}

#[derive(sqlx::FromRow)]
pub(crate) struct PursuitRow {
    pub id: Option<String>,
    pub opened_at: i64,
    pub closed_at: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
    pub queries: String,
    pub sources: String,
    pub artifact_id: Option<String>,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub query_vec: Option<Vec<u8>>,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub vec_dim: i64,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub embed_model: Option<String>,
}

fn row_to_pursuit(r: PursuitRow) -> Pursuit {
    Pursuit {
        id: r.id.unwrap_or_default(),
        opened_at: r.opened_at,
        closed_at: r.closed_at,
        state: r.state,
        reason: r.reason,
        queries: serde_json::from_str(&r.queries).unwrap_or_default(),
        sources: serde_json::from_str(&r.sources).unwrap_or_default(),
        artifact_id: r.artifact_id,
    }
}

impl Store {
    pub async fn record_interaction(
        &self,
        artifact_id: &str,
        kind: &str,
        via: Option<&str>,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO interaction_events (artifact_id, kind, via, scope, at)
             VALUES (?, ?, ?, ?, ?)",
            artifact_id,
            kind,
            via,
            scope,
            at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The weakest signal there is, so the sweep uses it only as a tiebreak.
    pub async fn record_dwell(
        &self,
        artifact_id: &str,
        secs: i64,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO interaction_events (artifact_id, kind, detail, scope, at)
             VALUES (?, 'dwell', ?, ?, ?)",
            artifact_id,
            secs.to_string(),
            scope,
            at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Neither counts as an ordinary open: the context sweep weighs `recommended_open` at
    /// `recommend.self_weight` and ignores `recommended_shown`; the pursuit sweep skips it too.
    pub async fn record_recommendation(
        &self,
        artifact_id: &str,
        kind: &str,
        detail: &str,
        scope: Option<&str>,
        at: i64,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO interaction_events (artifact_id, kind, detail, scope, at)
             VALUES (?, ?, ?, ?, ?)",
            artifact_id,
            kind,
            detail,
            scope,
            at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The instrument for later fitting the block weights, which are chosen, not measured.
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

    /// `from < at <= to`, oldest first.
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

    /// The same window as the situations: the sweep pairs the two, and `recommended_shown`
    /// rows grow with every page view.
    pub async fn expire_interactions(&self, retain_days: i64) -> Result<u64> {
        let cutoff = crate::store::now() - retain_days * 86_400;
        Ok(
            sqlx::query!("DELETE FROM interaction_events WHERE at < ?", cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Idempotent by identity: the sweep advances its cursor only at the end, so a retry
    /// re-clusters the same events and must not duplicate them.
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
        sqlx::query!(
            "INSERT OR IGNORE INTO pursuits
               (id, opened_at, state, queries, sources, query_vec, vec_dim, embed_model)
             VALUES (?, ?, 'open', ?, ?, ?, ?, ?)",
            &id,
            opened_at,
            serde_json::to_string(queries).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(sources).unwrap_or_else(|_| "[]".into()),
            blob,
            dim,
            model
        )
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn close_pursuit(&self, id: &str, state: &str, reason: &str, at: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE pursuits SET state = ?, reason = ?, closed_at = ? WHERE id = ?",
            state,
            reason,
            at,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_pursuit_artifact(&self, id: &str, artifact_id: &str, at: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE pursuits SET artifact_id = ?, state = 'generated', closed_at = ? WHERE id = ?",
            artifact_id,
            at,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Kept off [`Pursuit`]: list readers do not need a 768-float blob per row.
    pub async fn pursuit_vec(&self, id: &str) -> Result<Option<(Vec<f32>, String)>> {
        let row = sqlx::query("SELECT query_vec, embed_model FROM pursuits WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let blob: Vec<u8> = row.get("query_vec");
        let model: Option<String> = row.get("embed_model");
        let v = crate::store::feedback::blob_to_vec(&blob);
        match model {
            Some(m) if !v.is_empty() => Ok(Some((v, m))),
            _ => Ok(None),
        }
    }

    pub async fn get_pursuit(&self, id: &str) -> Result<Pursuit> {
        let row = sqlx::query_as!(
            crate::store::pursuits::PursuitRow,
            "SELECT * FROM pursuits WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound)?;
        Ok(row_to_pursuit(row))
    }

    pub async fn recent_pursuits(&self, limit: i64) -> Result<Vec<Pursuit>> {
        let rows = sqlx::query_as!(
            crate::store::pursuits::PursuitRow,
            "SELECT * FROM pursuits ORDER BY opened_at DESC LIMIT ?",
            limit.max(0)
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pursuit).collect())
    }

    /// A day's sittings, oldest first — the day page's read.
    pub async fn pursuits_between(&self, from: i64, to: i64) -> Result<Vec<Pursuit>> {
        let rows = sqlx::query_as!(
            crate::store::pursuits::PursuitRow,
            "SELECT * FROM pursuits WHERE opened_at >= ? AND opened_at < ? ORDER BY opened_at",
            from,
            to
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pursuit).collect())
    }

    /// Not `recent_pursuits().len()`, which caps at the page size.
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

    pub async fn purge_pursuits(&self) -> Result<u64> {
        let a = sqlx::query!("DELETE FROM interaction_events")
            .execute(&self.pool)
            .await?
            .rows_affected();
        let b = sqlx::query!("DELETE FROM pursuits")
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
                    text: "a".into(),
                    ..Default::default()
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
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let a = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a".into(),
                    ..Default::default()
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
