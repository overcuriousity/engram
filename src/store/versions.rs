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
    /// The write, in one transaction: version `n` = the current text, the
    /// artifact's text/title/caveats replaced, `embed_rev` bumped, the journal
    /// row. All of it or none of it. Returns `(action_id, n)`.
    pub async fn condense_artifact(
        &self,
        id: &str,
        text: &str,
        title: Option<&str>,
        caveats: &[String],
        mut evidence: serde_json::Value,
    ) -> Result<(String, i64)> {
        let mut tx = self.pool.begin_with(IMMEDIATE).await?;
        let row = sqlx::query("SELECT text, title, caveats FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        let n: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM artifact_versions WHERE artifact_id = ?",
        )
        .bind(id)
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
        sqlx::query(
            "INSERT INTO artifact_versions (artifact_id, n, text, title, caveats, created_at, action_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(n)
        .bind(row.get::<String, _>("text"))
        .bind(row.get::<Option<String>, _>("title"))
        .bind(row.get::<String, _>("caveats"))
        .bind(now())
        .bind(&action_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE artifacts SET text = ?, title = ?, caveats = ?,
                    embed_state = 'pending', embed_model = NULL,
                    embed_rev = embed_rev + 1, updated_at = ?
              WHERE id = ?",
        )
        .bind(text)
        .bind(title)
        .bind(serde_json::to_string(caveats).unwrap_or_else(|_| "[]".into()))
        .bind(now())
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((action_id, n))
    }

    /// Oldest first.
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

    /// Put version `n` back as the live text; bumps `embed_rev`. The caller
    /// stamps the journal row. The version row stays: nothing is deleted.
    pub async fn restore_version(&self, artifact_id: &str, n: i64) -> Result<()> {
        let row = sqlx::query(
            "SELECT text, title, caveats FROM artifact_versions WHERE artifact_id = ? AND n = ?",
        )
        .bind(artifact_id)
        .bind(n)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound)?;
        let res = sqlx::query(
            "UPDATE artifacts SET text = ?, title = ?, caveats = ?,
                    embed_state = 'pending', embed_model = NULL,
                    embed_rev = embed_rev + 1, updated_at = ?
              WHERE id = ?",
        )
        .bind(row.get::<String, _>("text"))
        .bind(row.get::<Option<String>, _>("title"))
        .bind(row.get::<String, _>("caveats"))
        .bind(now())
        .bind(artifact_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
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
            .condense_artifact(&id, "1.21.4", Some("Short"), &[], serde_json::json!({}))
            .await
            .unwrap();
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
            .condense_artifact(&id, "1.21", None, &[], serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(n2, 2);

        store.restore_version(&id, 1).await.unwrap();
        let back = store.get_artifact(&id).await.unwrap();
        assert_eq!(back.text, before.text);
        assert_eq!(back.title, before.title);
        assert_eq!(back.caveats, before.caveats);
        assert_eq!(
            store.versions_of(&id).await.unwrap().len(),
            2,
            "nothing deleted"
        );
    }
}
