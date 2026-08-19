//! What happens after a result list renders, and the pursuits it is grouped
//! into at analysis.
//!
//! An interaction carries no pursuit id: the clustering decides which pursuit
//! a search event belongs to, and an interaction is joined to that event by
//! time and scope when the sweep runs. Re-clustering never rewrites these rows.

use super::Store;
use crate::error::{Error, Result};
use sqlx::Row;

/// One thing done with a result: opened, or reached from another artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct Interaction {
    pub id: i64,
    pub artifact_id: String,
    /// `opened` | `pivoted`
    pub kind: String,
    /// The artifact this was reached from, for `pivoted`.
    pub via: Option<String>,
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

    /// Interactions with `from < at <= to`, oldest first.
    pub async fn interactions_between(&self, from: i64, to: i64) -> Result<Vec<Interaction>> {
        let rows = sqlx::query(
            "SELECT id, artifact_id, kind, via, scope, at FROM interaction_events
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
                scope: r.get("scope"),
                at: r.get("at"),
            })
            .collect())
    }

    /// A new pursuit, `open`. Returns its id.
    pub async fn insert_pursuit(
        &self,
        opened_at: i64,
        queries: &[String],
        sources: &[String],
    ) -> Result<String> {
        let id = super::new_id();
        sqlx::query(
            "INSERT INTO pursuits (id, opened_at, state, queries, sources)
             VALUES (?, ?, 'open', ?, ?)",
        )
        .bind(&id)
        .bind(opened_at)
        .bind(serde_json::to_string(queries).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(sources).unwrap_or_else(|_| "[]".into()))
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
    }

    #[tokio::test]
    async fn a_pursuit_round_trips_and_closes() {
        let s = Store::memory().await.unwrap();
        let id = s
            .insert_pursuit(100, &["how to mount".into()], &["a1".into(), "a2".into()])
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
