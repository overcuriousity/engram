//! Page-view situations and the clusters the sweep learns from them. Nothing is joined by a
//! stored id, so a sweep under a new encoder starts from the raw bundles.

use super::{Store, now};
use crate::error::Result;
use crate::store::feedback::{blob_to_vec, vec_to_blob};
use sqlx::Row;
use std::collections::HashMap;

/// Not a setting: weekly and monthly patterns need this long a window to be learnt at all.
pub const RETAIN_DAYS: i64 = 400;

#[derive(Debug, Clone, Default)]
pub struct ContextEvent {
    pub id: i64,
    pub scope: Option<String>,
    pub at: i64,
    pub bundle: String,
    pub device_key: Option<String>,
    /// The fractional local hour — 14.9 for 14:55, as the encoder reads it.
    pub local_hour: Option<f64>,
    pub weekday: Option<i64>,
    pub tz: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredCluster {
    pub scope: Option<String>,
    pub artifact_id: String,
    pub slot: i64,
    pub centroid: Vec<f32>,
    pub weight: f64,
    /// Undecayed. See the column comment.
    pub events: i64,
    pub last_at: i64,
    pub encoder_version: i64,
    pub representative: String,
}

impl Store {
    pub async fn record_context(&self, ev: &ContextEvent) -> Result<i64> {
        let res = sqlx::query!(
            "INSERT INTO context_events
                 (scope, at, bundle, device_key, local_hour, weekday, tz)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &ev.scope,
            ev.at,
            &ev.bundle,
            &ev.device_key,
            ev.local_hour,
            ev.weekday,
            &ev.tz
        )
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// Unbounded on purpose, bounded by `RETAIN_DAYS`: the sweep rebuilds from the whole window.
    pub async fn context_events_since(&self, since: i64) -> Result<Vec<ContextEvent>> {
        let rows = sqlx::query!(
            "SELECT id, scope, at, bundle, device_key, local_hour, weekday, tz
               FROM context_events
              WHERE at >= ?
              ORDER BY at, id",
            since
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ContextEvent {
                id: r.id,
                scope: r.scope,
                at: r.at,
                bundle: r.bundle,
                device_key: r.device_key,
                local_hour: r.local_hour,
                weekday: r.weekday,
                tz: r.tz,
            })
            .collect())
    }

    pub async fn expire_context_events(&self, retain_days: i64) -> Result<u64> {
        let cutoff = now() - retain_days * 86_400;
        Ok(
            sqlx::query!("DELETE FROM context_events WHERE at < ?", cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Not `resurface`: that returns nothing on a new base and marks what it draws as seen.
    pub async fn random_artifact(&self) -> Result<Option<(String, Option<String>, String)>> {
        let row = sqlx::query!(
            r#"SELECT id AS "id!", title, text FROM artifacts
              WHERE status = 'active' AND superseded_by IS NULL
              ORDER BY RANDOM() LIMIT 1"#
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.id, r.title, r.text)))
    }

    /// The sweep needs these to clear a profile whose clusters all decayed below `min_weight`.
    pub async fn artifacts_with_context_clusters(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar!("SELECT DISTINCT artifact_id FROM context_clusters")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Wholesale, never merged: the multivector must match the table. An empty list clears.
    pub async fn replace_context_clusters(
        &self,
        artifact_id: &str,
        clusters: &[StoredCluster],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "DELETE FROM context_clusters WHERE artifact_id = ?",
            artifact_id
        )
        .execute(&mut *tx)
        .await?;
        for c in clusters {
            sqlx::query!(
                "INSERT INTO context_clusters
                     (scope, artifact_id, slot, centroid, weight, events,
                      last_at, encoder_version, representative)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &c.scope,
                artifact_id,
                c.slot,
                vec_to_blob(&c.centroid),
                c.weight,
                c.events,
                c.last_at,
                c.encoder_version,
                &c.representative
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Ids with no clusters are absent rather than empty.
    pub async fn context_clusters_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<HashMap<String, Vec<StoredCluster>>> {
        if artifact_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // sqlx has no list binding for SQLite; only placeholders are generated, values are bound.
        let holes = std::iter::repeat_n("?", artifact_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT scope, artifact_id, slot, centroid, weight, events, last_at,
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
                    events: r.get("events"),
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
            local_hour: Some(15.0),
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

        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &aid)
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
            events: 3,
            last_at: 1_000,
            encoder_version: 1,
            representative: r#"{"at":1000,"bundle":{}}"#.into(),
        }
    }

    async fn seed_artifact(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a".into(),
                    corpus_span: Some(CorpusSpan {
                        start_line: 1,
                        end_line: 2,
                        source: crate::store::artifacts::SpanSource::Located,
                    }),
                    title: Some("t".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }
}
