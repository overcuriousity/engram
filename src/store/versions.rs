//! The versions a condensation retired: the live text stays in `artifacts`,
//! every earlier one is here, readable in place and one call from live.

use super::{Store, now};
use crate::error::{Error, Result};
use sqlx::Row;

const IMMEDIATE: &str = "BEGIN IMMEDIATE";

#[derive(Debug, Clone)]
pub struct Version {
    pub artifact_id: String,
    pub n: i64,
    pub text: String,
    pub title: Option<String>,
    pub caveats: Vec<String>,
    pub created_at: i64,
    pub action_id: String,
}

impl Store {
    /// Returns `(action_id, n)`, or `Ok(None)` when `embed_rev` no longer
    /// equals `expected_rev`: the model call is slow, and an edit made meanwhile must survive.
    pub async fn condense_artifact(
        &self,
        id: &str,
        expected_rev: Option<i64>,
        text: &str,
        title: Option<&str>,
        caveats: &[String],
        mut evidence: serde_json::Value,
    ) -> Result<Option<(String, i64)>> {
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let row = sqlx::query("SELECT text, title, caveats, embed_rev FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        // Checked before any write, so no version or journal row is minted for a discarded rewrite.
        if let Some(want) = expected_rev
            && row.get::<i64, _>("embed_rev") != want
        {
            return Ok(None);
        }
        let n: i64 = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM artifact_versions WHERE artifact_id = ?",
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if let Some(obj) = evidence.as_object_mut() {
            obj.insert("version".into(), serde_json::json!(n));
        }
        let action_id = super::actions::insert(
            &mut *tx,
            &super::actions::NewAction {
                job: super::actions::Job::Sleep,
                kind: super::actions::Kind::Condense,
                subject_id: id.to_string(),
                survivor_id: Some(id.to_string()),
                detail: Some(format!("version {n} retired")),
                evidence,
                pair_score: None,
            },
        )
        .await?;
        sqlx::query!(
            "INSERT INTO artifact_versions (artifact_id, n, text, title, caveats, created_at, action_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            id,
            n,
            row.get::<String, _>("text"),
            row.get::<Option<String>, _>("title"),
            row.get::<String, _>("caveats"),
            now(),
            &action_id
        )
            .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE artifacts SET text = ?, title = ?, caveats = ?,
                    embed_state = 'pending', embed_model = NULL,
                    embed_rev = embed_rev + 1, updated_at = ?
              WHERE id = ?",
            text,
            title,
            serde_json::to_string(caveats).unwrap_or_else(|_| "[]".into()),
            now(),
            id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some((action_id, n)))
    }

    pub async fn versions_of(&self, artifact_id: &str) -> Result<Vec<Version>> {
        let rows = sqlx::query(
            "SELECT artifact_id, n, text, title, caveats, created_at, action_id
               FROM artifact_versions WHERE artifact_id = ? ORDER BY n ASC",
        )
        .bind(artifact_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Version {
                artifact_id: r.get("artifact_id"),
                n: r.get("n"),
                text: r.get("text"),
                title: r.get("title"),
                caveats: serde_json::from_str(&r.get::<String, _>("caveats")).unwrap_or_default(),
                created_at: r.get("created_at"),
                action_id: r.get("action_id"),
            })
            .collect())
    }

    /// The text being replaced becomes a version first, retired by `action_id`, so an edit
    /// made after a condensation survives a restore.
    pub async fn restore_version(&self, artifact_id: &str, n: i64, action_id: &str) -> Result<()> {
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let row = sqlx::query(
            "SELECT text, title, caveats FROM artifact_versions WHERE artifact_id = ? AND n = ?",
        )
        .bind(artifact_id)
        .bind(n)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
        let current = sqlx::query("SELECT text, title, caveats FROM artifacts WHERE id = ?")
            .bind(artifact_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        sqlx::query!(
            "INSERT INTO artifact_versions (artifact_id, n, text, title, caveats, created_at, action_id)
             SELECT ?, COALESCE(MAX(n), 0) + 1, ?, ?, ?, ?, ?
               FROM artifact_versions WHERE artifact_id = ?",
            artifact_id,
            current.get::<String, _>("text"),
            current.get::<Option<String>, _>("title"),
            current.get::<String, _>("caveats"),
            now(),
            action_id,
            artifact_id
        )
            .execute(&mut *tx)
        .await?;
        let res = sqlx::query!(
            "UPDATE artifacts SET text = ?, title = ?, caveats = ?,
                    embed_state = 'pending', embed_model = NULL,
                    embed_rev = embed_rev + 1, updated_at = ?
              WHERE id = ?",
            row.get::<String, _>("text"),
            row.get::<Option<String>, _>("title"),
            row.get::<String, _>("caveats"),
            now(),
            artifact_id
        )
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::actions::Kind;

    async fn one(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "long text with 1.21.4 in it".into(),
                    title: Some("Long".into()),
                    caveats: vec!["only on ext4".into()],
                    ..Default::default()
                }],
            )
            .await
            .unwrap()[0]
            .id
            .clone()
    }

    #[tokio::test]
    async fn a_condensation_writes_the_version_the_text_and_the_row_together_and_restores_byte_for_byte()
     {
        let store = Store::memory().await.unwrap();
        let id = one(&store).await;
        let before = store.get_artifact(&id).await.unwrap();
        let (action, n) = store
            .condense_artifact(
                &id,
                Some(before.embed_rev),
                "1.21.4",
                Some("Short"),
                &[],
                serde_json::json!({}),
            )
            .await
            .unwrap()
            .expect("the revision is the one just read");
        assert_eq!(n, 1);
        let after = store.get_artifact(&id).await.unwrap();
        assert_eq!(after.text, "1.21.4");
        assert_eq!(after.title.as_deref(), Some("Short"));
        assert!(after.caveats.is_empty());
        assert_eq!(after.embed_rev, before.embed_rev + 1);
        let v = store.versions_of(&id).await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, before.text);
        assert_eq!(v[0].caveats, before.caveats);
        assert_eq!(v[0].action_id, action);
        let a = store.action(&action).await.unwrap().unwrap();
        assert_eq!(a.kind, Kind::Condense);
        assert!(
            a.evidence_json.contains("\"version\":1"),
            "{}",
            a.evidence_json
        );

        let (_, n2) = store
            .condense_artifact(&id, None, "1.21", None, &[], serde_json::json!({}))
            .await
            .unwrap()
            .expect("no revision pinned");
        assert_eq!(n2, 2);

        store.restore_version(&id, 1, &action).await.unwrap();
        let back = store.get_artifact(&id).await.unwrap();
        assert_eq!(back.text, before.text);
        assert_eq!(back.title, before.title);
        assert_eq!(back.caveats, before.caveats);
        assert_eq!(
            store.versions_of(&id).await.unwrap().len(),
            3,
            "nothing deleted, including the text the restore replaced"
        );
    }

    #[tokio::test]
    async fn restoring_a_version_keeps_the_text_it_replaces() {
        let store = Store::memory().await.unwrap();
        let id = one(&store).await;
        let before = store.get_artifact(&id).await.unwrap();
        let (action, n) = store
            .condense_artifact(&id, None, "1.21.4", None, &[], serde_json::json!({}))
            .await
            .unwrap()
            .expect("no revision pinned");
        store
            .update_artifact_text(&id, "1.21.4, and only on ext4")
            .await
            .unwrap();

        store.restore_version(&id, n, &action).await.unwrap();
        assert_eq!(store.get_artifact(&id).await.unwrap().text, before.text);
        let kept = store.versions_of(&id).await.unwrap();
        assert!(
            kept.iter()
                .any(|v| v.text == "1.21.4, and only on ext4" && v.action_id == action),
            "the edit was written over and kept nowhere: {kept:?}"
        );
    }
}
