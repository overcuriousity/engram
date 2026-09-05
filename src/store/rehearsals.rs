//! Probes: questions the base can ask itself about one artifact.
//!
//! Two classes and a rule. A `capture` probe is the text of a later capture
//! that landed near this artifact at integration — a query written by a
//! person who was not looking at the answer, which is the standard verdicts
//! are held to. A `cue` probe is a question a model-written artifact was
//! written for. The rule: nothing is ever minted from an artifact's own
//! title, body or tags, because a query written while looking at the answer
//! passes on every system ever built.

use super::feedback::{blob_to_vec, vec_to_blob};
use super::{Cursor, Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Capture,
    Cue,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Capture => "capture",
            Class::Cue => "cue",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "capture" => Class::Capture,
            "cue" => Class::Cue,
            other => return Err(Error::Store(format!("rehearsals: unknown class {other}"))),
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewRehearsal {
    pub class: Class,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub artifact_id: String,
    pub source_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Rehearsal {
    pub id: String,
    pub created_at: i64,
    pub class: Class,
    pub query: String,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub artifact_id: String,
    pub source_id: Option<String>,
    pub retired_at: Option<i64>,
}

const COLUMNS: &str =
    "id, created_at, class, query, query_vec, embed_model, artifact_id, source_id, retired_at";

fn read(r: &sqlx::sqlite::SqliteRow) -> Result<Rehearsal> {
    Ok(Rehearsal {
        id: r.get("id"),
        created_at: r.get("created_at"),
        class: Class::parse(&r.get::<String, _>("class"))?,
        query: r.get("query"),
        query_vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
        embed_model: r.get("embed_model"),
        artifact_id: r.get("artifact_id"),
        source_id: r.get("source_id"),
        retired_at: r.get("retired_at"),
    })
}

impl Store {
    /// `None` when the same (artifact, class, query) already exists: a probe
    /// is written once, however often the unit that mints it runs.
    pub async fn record_rehearsal(&self, r: &NewRehearsal) -> Result<Option<String>> {
        let id = new_id();
        let res = sqlx::query(
            "INSERT OR IGNORE INTO rehearsals
               (id, created_at, class, query, query_vec, vec_dim, embed_model, artifact_id, source_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(now())
        .bind(r.class.as_str())
        .bind(&r.query)
        .bind(vec_to_blob(&r.query_vec))
        .bind(r.query_vec.len() as i64)
        .bind(&r.embed_model)
        .bind(&r.artifact_id)
        .bind(&r.source_id)
        .execute(&self.pool)
        .await?;
        Ok((res.rows_affected() > 0).then_some(id))
    }

    pub async fn rehearsal(&self, id: &str) -> Result<Option<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(read)
        .transpose()
    }

    /// Live probes after `cursor`, in `(created_at, id)` order. The pair and
    /// not a bare stamp: the clock is seconds, and a limit that cuts inside
    /// one second would otherwise lose the rest of it.
    pub async fn rehearsals_after(&self, cursor: &Cursor, limit: usize) -> Result<Vec<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals
              WHERE retired_at IS NULL
                AND (created_at > ? OR (created_at = ? AND id > ?))
              ORDER BY created_at ASC, id ASC
              LIMIT ?"
        )))
        .bind(cursor.at)
        .bind(cursor.at)
        .bind(&cursor.id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    /// Live probes for one artifact, oldest first.
    pub async fn rehearsals_of(&self, artifact_id: &str) -> Result<Vec<Rehearsal>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM rehearsals
              WHERE artifact_id = ? AND retired_at IS NULL
              ORDER BY created_at ASC, id ASC"
        )))
        .bind(artifact_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }

    pub async fn retire_rehearsal(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE rehearsals SET retired_at = ? WHERE id = ? AND retired_at IS NULL")
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Retire every live probe of this artifact. Rows stamped.
    pub async fn retire_rehearsals_of(&self, artifact_id: &str, at: i64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE rehearsals SET retired_at = ? WHERE artifact_id = ? AND retired_at IS NULL",
        )
        .bind(at)
        .bind(artifact_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn live_rehearsal_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM rehearsals WHERE retired_at IS NULL")
                .fetch_one(&self.pool)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    pub(crate) async fn owner(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let new = vec![crate::store::artifacts::NewArtifact {
            ordinal: 0,
            text: "the image will not mount".into(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        }];
        store.insert_artifacts(&src.id, &new).await.unwrap()[0]
            .id
            .clone()
    }

    pub(crate) fn probe(owner: &str, q: &str) -> NewRehearsal {
        NewRehearsal {
            class: Class::Capture,
            query: q.into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            artifact_id: owner.into(),
            source_id: Some("src".into()),
        }
    }

    #[tokio::test]
    async fn a_probe_is_recorded_once_and_walked_on_a_cursor_until_retired() {
        let store = Store::memory().await.unwrap();
        let o = owner(&store).await;
        let a = store
            .record_rehearsal(&probe(&o, "why won't it mount"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .record_rehearsal(&probe(&o, "why won't it mount"))
                .await
                .unwrap()
                .is_none()
        );
        let b = store
            .record_rehearsal(&probe(&o, "mount fails on boot"))
            .await
            .unwrap()
            .unwrap();

        let all = store
            .rehearsals_after(&Cursor::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [a.as_str(), b.as_str()]
        );
        let read = store.rehearsal(&a).await.unwrap().unwrap();
        assert_eq!(read.query_vec, vec![0.1, 0.2, 0.3]);
        assert_eq!(read.class, Class::Capture);

        let after_a = Cursor {
            at: read.created_at,
            id: a.clone(),
        };
        let rest = store.rehearsals_after(&after_a, 10).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, b);

        store.retire_rehearsal(&a, 5).await.unwrap();
        assert_eq!(store.live_rehearsal_count().await.unwrap(), 1);
        assert_eq!(
            store.rehearsals_of(&o).await.unwrap().len(),
            1,
            "retired probes are not listed"
        );
        assert_eq!(store.retire_rehearsals_of(&o, 6).await.unwrap(), 1);
        assert_eq!(store.live_rehearsal_count().await.unwrap(), 0);
    }
}
