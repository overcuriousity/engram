//! What situation a page view happened in, and what the sweep made of them.
//!
//! Two tables with one rule between them: nothing here is joined by a stored
//! id. A context event is matched to an open through `scope` and `at`, the way
//! `interaction_events` is matched to a pursuit — so re-clustering never has to
//! rewrite a row, and a sweep run under a new encoder starts from the raw
//! bundles rather than from what the last one concluded.

use super::{Store, now};
use crate::error::Result;
use crate::store::feedback::{blob_to_vec, vec_to_blob};
use sqlx::Row;
use std::collections::HashMap;

/// How long a situation is kept.
///
/// Not `feedback.retain_days`, and not a setting. A weekly pattern needs weeks
/// and a monthly one needs months, so the window this feature needs is not the
/// window an operator sets for their query log — and it is longer than either
/// default. Housekeeping about how long the base may remember a Friday
/// afternoon is not a preference; it is what the feature costs to work at all.
pub const RETAIN_DAYS: i64 = 400;

/// One page view, as the browser described it.
#[derive(Debug, Clone, Default)]
pub struct ContextEvent {
    pub id: i64,
    pub scope: Option<String>,
    pub at: i64,
    /// The bundle as received, JSON.
    pub bundle: String,
    pub device_key: Option<String>,
    pub local_hour: Option<i64>,
    pub weekday: Option<i64>,
    pub tz: Option<String>,
}

/// One learned situation, as SQLite holds it.
#[derive(Debug, Clone)]
pub struct StoredCluster {
    pub scope: Option<String>,
    pub artifact_id: String,
    pub slot: i64,
    pub centroid: Vec<f32>,
    pub weight: f64,
    pub last_at: i64,
    pub encoder_version: i64,
    pub representative: String,
}

impl Store {
    /// Record one page view's situation. Returns the row's own id.
    pub async fn record_context(&self, ev: &ContextEvent) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO context_events
                 (scope, at, bundle, device_key, local_hour, weekday, tz)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&ev.scope)
        .bind(ev.at)
        .bind(&ev.bundle)
        .bind(&ev.device_key)
        .bind(ev.local_hour)
        .bind(ev.weekday)
        .bind(&ev.tz)
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// Every situation recorded at or after `since`, oldest first.
    ///
    /// Unbounded on purpose, and bounded in practice by `RETAIN_DAYS`: the one
    /// caller is the sweep, which rebuilds every profile from the raw bundles
    /// and therefore needs all of them. Paging it would mean holding a cursor
    /// across a rebuild that is only correct when it sees the whole window.
    pub async fn context_events_since(&self, since: i64) -> Result<Vec<ContextEvent>> {
        let rows = sqlx::query(
            "SELECT id, scope, at, bundle, device_key, local_hour, weekday, tz
               FROM context_events
              WHERE at >= ?
              ORDER BY at, id",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| ContextEvent {
                id: r.get("id"),
                scope: r.get("scope"),
                at: r.get("at"),
                bundle: r.get("bundle"),
                device_key: r.get("device_key"),
                local_hour: r.get("local_hour"),
                weekday: r.get("weekday"),
                tz: r.get("tz"),
            })
            .collect())
    }

    /// Drop situations past keeping. See `RETAIN_DAYS`.
    pub async fn expire_context_events(&self, retain_days: i64) -> Result<u64> {
        let cutoff = now() - retain_days * 86_400;
        Ok(sqlx::query("DELETE FROM context_events WHERE at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Every artifact that currently has a profile.
    ///
    /// What the sweep needs in order to *clear* one: an artifact whose every
    /// situation has decayed below `min_weight` produces no clusters this run,
    /// so nothing in the run's own output names it, and without this its old
    /// centroids would stand for ever — offering it on a pattern that stopped
    /// months ago.
    pub async fn artifacts_with_context_clusters(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT DISTINCT artifact_id FROM context_clusters")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Replace everything this artifact has learned, in one transaction.
    ///
    /// Wholesale, never merged. The sweep rebuilds a profile from the raw
    /// bundles every run, so a merge would leave a slot from a previous run
    /// standing beside fresh ones — and the multivector written from the fresh
    /// ones would then not match the table the reason is read out of. An empty
    /// list is a clear, which is what an artifact whose every cluster fell
    /// below `min_weight` needs.
    pub async fn replace_context_clusters(
        &self,
        artifact_id: &str,
        clusters: &[StoredCluster],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM context_clusters WHERE artifact_id = ?")
            .bind(artifact_id)
            .execute(&mut *tx)
            .await?;
        for c in clusters {
            sqlx::query(
                "INSERT INTO context_clusters
                     (scope, artifact_id, slot, centroid, weight, last_at,
                      encoder_version, representative)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&c.scope)
            .bind(artifact_id)
            .bind(c.slot)
            .bind(vec_to_blob(&c.centroid))
            .bind(c.weight)
            .bind(c.last_at)
            .bind(c.encoder_version)
            .bind(&c.representative)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// What these artifacts have learned, keyed by artifact, in slot order.
    /// Ids with no clusters are absent from the answer rather than present and
    /// empty — the read path asks for the ten the vector store returned and
    /// needs to know which of them it can say nothing about.
    pub async fn context_clusters_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<HashMap<String, Vec<StoredCluster>>> {
        if artifact_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Built by hand because sqlx has no list binding for SQLite, and
        // `AssertSqlSafe` because the string is assembled here — the values are
        // bound and never spliced, and only the placeholders are generated.
        // Same shape as `artifacts_by_ids`.
        let holes = std::iter::repeat_n("?", artifact_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT scope, artifact_id, slot, centroid, weight, last_at,
                    encoder_version, representative
               FROM context_clusters
              WHERE artifact_id IN ({holes})
              ORDER BY artifact_id, slot"
        )));
        for id in artifact_ids {
            q = q.bind(id);
        }
        let mut out: HashMap<String, Vec<StoredCluster>> = HashMap::new();
        for r in q.fetch_all(&self.pool).await? {
            let artifact_id: String = r.get("artifact_id");
            out.entry(artifact_id.clone())
                .or_default()
                .push(StoredCluster {
                    scope: r.get("scope"),
                    artifact_id,
                    slot: r.get("slot"),
                    centroid: blob_to_vec(&r.get::<Vec<u8>, _>("centroid")),
                    weight: r.get("weight"),
                    last_at: r.get("last_at"),
                    encoder_version: r.get("encoder_version"),
                    representative: r.get("representative"),
                });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{CorpusSpan, NewArtifact};

    fn event(scope: &str, at: i64) -> ContextEvent {
        ContextEvent {
            id: 0,
            scope: Some(scope.into()),
            at,
            bundle: r#"{"tz":"Europe/Berlin"}"#.into(),
            device_key: Some("phone".into()),
            local_hour: Some(15),
            weekday: Some(4),
            tz: Some("Europe/Berlin".into()),
        }
    }

    #[tokio::test]
    async fn a_recorded_situation_comes_back_whole() {
        let store = Store::memory().await.unwrap();
        store.record_context(&event("alice", 1_000)).await.unwrap();

        let out = store.context_events_since(0).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].scope.as_deref(), Some("alice"));
        assert_eq!(out[0].bundle, r#"{"tz":"Europe/Berlin"}"#);
        assert_eq!(out[0].weekday, Some(4));
        assert!(out[0].id > 0, "the row's own id, not the argument's");
    }

    #[tokio::test]
    async fn situations_come_back_oldest_first() {
        let store = Store::memory().await.unwrap();
        for at in [3_000, 1_000, 2_000] {
            store.record_context(&event("alice", at)).await.unwrap();
        }
        let ats: Vec<i64> = store
            .context_events_since(0)
            .await
            .unwrap()
            .iter()
            .map(|e| e.at)
            .collect();
        assert_eq!(ats, vec![1_000, 2_000, 3_000]);
    }

    #[tokio::test]
    async fn expiry_uses_this_features_own_window() {
        // Not `feedback.retain_days`: an operator who shortens their query log
        // is not asking the base to forget what Friday afternoon looks like.
        let store = Store::memory().await.unwrap();
        let day = 86_400;
        store
            .record_context(&event("alice", now() - 500 * day))
            .await
            .unwrap();
        store
            .record_context(&event("alice", now() - 10 * day))
            .await
            .unwrap();

        let dropped = store.expire_context_events(RETAIN_DAYS).await.unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(store.context_events_since(0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn clusters_are_replaced_wholesale_not_merged() {
        // The sweep is a full rebuild per artifact. A write that merged would
        // leave a slot from a previous encoder standing beside fresh ones, and
        // the multivector written from them would not match the table.
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;

        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 3.0), cluster(&aid, 1, 2.0)])
            .await
            .unwrap();
        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 9.0)])
            .await
            .unwrap();

        let back = store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap();
        let mine = &back[&aid];
        assert_eq!(mine.len(), 1, "slot 1 is gone, not stale");
        assert_eq!(mine[0].weight, 9.0);
    }

    #[tokio::test]
    async fn an_empty_list_clears_the_profile() {
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;
        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 3.0)])
            .await
            .unwrap();
        assert_eq!(
            store.artifacts_with_context_clusters().await.unwrap(),
            vec![aid.clone()]
        );

        store.replace_context_clusters(&aid, &[]).await.unwrap();
        assert!(
            store
                .artifacts_with_context_clusters()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_centroid_survives_the_round_trip() {
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;
        let mut c = cluster(&aid, 0, 1.0);
        c.centroid = vec![0.5, -0.25, 0.125];
        store.replace_context_clusters(&aid, &[c]).await.unwrap();

        let back = store
            .context_clusters_of(std::slice::from_ref(&aid))
            .await
            .unwrap();
        assert_eq!(back[&aid][0].centroid, vec![0.5, -0.25, 0.125]);
    }

    #[tokio::test]
    async fn an_artifact_with_no_clusters_is_absent_rather_than_empty() {
        // The read path asks for the ten ids the store returned and expects to
        // learn which of them it knows nothing about.
        let store = Store::memory().await.unwrap();
        let back = store
            .context_clusters_of(&["nobody".to_string()])
            .await
            .unwrap();
        assert!(back.is_empty());
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_situations_with_it() {
        let store = Store::memory().await.unwrap();
        let aid = seed_artifact(&store).await;
        store
            .replace_context_clusters(&aid, &[cluster(&aid, 0, 1.0)])
            .await
            .unwrap();

        sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(&aid)
            .execute(&store.pool)
            .await
            .unwrap();

        let back = store.context_clusters_of(&[aid]).await.unwrap();
        assert!(back.is_empty(), "ON DELETE CASCADE");
    }

    fn cluster(artifact_id: &str, slot: i64, weight: f64) -> StoredCluster {
        StoredCluster {
            scope: Some("alice".into()),
            artifact_id: artifact_id.into(),
            slot,
            centroid: vec![1.0, 0.0],
            weight,
            last_at: 1_000,
            encoder_version: 1,
            representative: r#"{"at":1000,"bundle":{}}"#.into(),
        }
    }

    /// A corpus and one artifact in it, because `context_clusters.artifact_id`
    /// is a foreign key and SQLite enforces it.
    async fn seed_artifact(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
                    corpus_span: Some(CorpusSpan {
                        start_line: 1,
                        end_line: 2,
                    }),
                    caveats: vec![],
                    title: Some("t".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }
}
