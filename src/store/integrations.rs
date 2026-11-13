//! What the base found when a new artifact arrived: its salience, as a word.

use super::Store;
use super::artifacts::Chunk;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Novel,
    Known,
}

impl Tag {
    pub fn as_str(self) -> &'static str {
        match self {
            Tag::Novel => "novel",
            Tag::Known => "known",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "novel" => Tag::Novel,
            // `conflict` rows predate the retired detector; they read as `Known`.
            "known" | "conflict" => Tag::Known,
            other => return Err(Error::Store(format!("integrations: unknown tag {other}"))),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Integration {
    pub artifact_id: String,
    pub at: i64,
    pub tag: Tag,
    pub nearest_id: Option<String>,
    pub nearest_score: Option<f32>,
    pub detail: Option<String>,
}

impl Store {
    /// `false` when the artifact already has a row. A tag is written once.
    pub async fn record_integration(&self, i: &Integration) -> Result<bool> {
        let res = sqlx::query!(
            "INSERT OR IGNORE INTO integrations
               (artifact_id, at, tag, nearest_id, nearest_score, detail)
             VALUES (?, ?, ?, ?, ?, ?)",
            &i.artifact_id,
            i.at,
            i.tag.as_str(),
            &i.nearest_id,
            i.nearest_score.map(f64::from),
            &i.detail
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn integration_of(&self, artifact_id: &str) -> Result<Option<Integration>> {
        let row = sqlx::query!(
            r#"SELECT artifact_id AS "artifact_id!", at, tag, nearest_id, nearest_score, detail
               FROM integrations WHERE artifact_id = ?"#,
            artifact_id
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(Integration {
                artifact_id: r.artifact_id,
                at: r.at,
                tag: Tag::parse(&r.tag)?,
                nearest_id: r.nearest_id,
                nearest_score: r.nearest_score.map(|s| s as f32),
                detail: r.detail,
            })
        })
        .transpose()
    }

    /// Only artifacts in results: a hidden one would be re-scanned every pass for nothing.
    pub async fn artifacts_to_integrate(&self, limit: usize) -> Result<Vec<Chunk>> {
        let ids: Vec<String> = sqlx::query_scalar!(
            r#"SELECT a.id AS "id!" FROM artifacts a
               LEFT JOIN integrations i ON i.artifact_id = a.id
              WHERE i.artifact_id IS NULL
                AND a.embed_state = 'embedded'
                AND a.status = 'active' AND a.superseded_by IS NULL AND a.reaped_at IS NULL
              ORDER BY a.created_at ASC, a.id ASC
              LIMIT ?"#,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?;
        let rows = self.artifacts_by_ids(&ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| rows.iter().find(|c| &c.id == id).cloned())
            .collect())
    }

    /// (novel, known, conflict) over every row.
    pub async fn integration_counts(&self) -> Result<(i64, i64, i64)> {
        let r = sqlx::query!(
            "SELECT COALESCE(SUM(tag = 'novel'), 0) AS novel,
                    COALESCE(SUM(tag = 'known'), 0) AS known,
                    COALESCE(SUM(tag = 'conflict'), 0) AS conflict
               FROM integrations"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((r.novel, r.known, r.conflict))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[tokio::test]
    async fn a_tag_is_written_once_and_the_work_list_is_what_has_none() {
        let store = Store::memory().await.unwrap();
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<_> = (0..2)
            .map(|i| crate::store::artifacts::NewArtifact {
                ordinal: i,
                text: format!("text {i}"),
                ..Default::default()
            })
            .collect();
        let ids: Vec<String> = store
            .insert_artifacts(&src.id, &new)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(store.artifacts_to_integrate(10).await.unwrap().is_empty());
        for id in &ids {
            assert!(store.mark_embedded(id, "fake", 0).await.unwrap());
        }
        assert_eq!(store.artifacts_to_integrate(10).await.unwrap().len(), 2);

        let row = Integration {
            artifact_id: ids[0].clone(),
            at: 1,
            tag: Tag::Novel,
            nearest_id: None,
            nearest_score: None,
            detail: None,
        };
        assert!(store.record_integration(&row).await.unwrap());
        assert!(
            !store
                .record_integration(&Integration {
                    tag: Tag::Known,
                    ..row.clone()
                })
                .await
                .unwrap()
        );
        assert_eq!(
            store.integration_of(&ids[0]).await.unwrap().unwrap().tag,
            Tag::Novel
        );
        assert_eq!(store.artifacts_to_integrate(10).await.unwrap().len(), 1);
        assert_eq!(store.integration_counts().await.unwrap(), (1, 0, 0));
    }
}
