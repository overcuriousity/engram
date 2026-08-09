use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceStatus {
    Raw,
    Segmenting,
    Segmented,
    Embedding,
    Ready,
    Partial,
    Failed,
}

impl SourceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceStatus::Raw => "raw",
            SourceStatus::Segmenting => "segmenting",
            SourceStatus::Segmented => "segmented",
            SourceStatus::Embedding => "embedding",
            SourceStatus::Ready => "ready",
            SourceStatus::Partial => "partial",
            SourceStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> SourceStatus {
        match s {
            "segmenting" => SourceStatus::Segmenting,
            "segmented" => SourceStatus::Segmented,
            "embedding" => SourceStatus::Embedding,
            "ready" => SourceStatus::Ready,
            "partial" => SourceStatus::Partial,
            "failed" => SourceStatus::Failed,
            _ => SourceStatus::Raw,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Source {
    pub id: String,
    pub raw_text: String,
    pub origin: String,
    pub title_hint: Option<String>,
    pub content_hash: String,
    pub status: SourceStatus,
    pub created_at: i64,
    pub updated_at: i64,
    /// Fraction of this source's non-blank lines that ended up inside some
    /// chunk. `None` for sources segmented before the check existed.
    pub coverage: Option<f64>,
}

pub fn content_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn row_to_source(r: &sqlx::sqlite::SqliteRow) -> Source {
    Source {
        id: r.get("id"),
        raw_text: r.get("raw_text"),
        origin: r.get("origin"),
        title_hint: r.get("title_hint"),
        content_hash: r.get("content_hash"),
        status: SourceStatus::parse(r.get::<String, _>("status").as_str()),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        coverage: r.get("coverage"),
    }
}

impl Store {
    pub async fn insert_source(
        &self,
        raw_text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<Source> {
        let src = Source {
            id: new_id(),
            raw_text: raw_text.to_string(),
            origin: origin.to_string(),
            title_hint: title_hint.map(str::to_string),
            content_hash: content_hash(raw_text),
            status: SourceStatus::Raw,
            created_at: now(),
            updated_at: now(),
            coverage: None,
        };
        sqlx::query(
            "INSERT INTO sources (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&src.id)
        .bind(&src.raw_text)
        .bind(&src.origin)
        .bind(&src.title_hint)
        .bind(&src.content_hash)
        .bind(src.status.as_str())
        .bind(src.created_at)
        .bind(src.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(src)
    }

    pub async fn get_source(&self, id: &str) -> Result<Source> {
        let row = sqlx::query("SELECT * FROM sources WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_source(&row))
    }

    pub async fn find_by_hash(&self, hash: &str) -> Result<Option<Source>> {
        let row = sqlx::query("SELECT * FROM sources WHERE content_hash = ?")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_source))
    }

    pub async fn set_source_status(&self, id: &str, status: SourceStatus) -> Result<()> {
        sqlx::query("UPDATE sources SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// How much of this source ended up inside a chunk. Written once every
    /// window has resolved; a low number means the segmenter dropped part of
    /// the document, which nothing used to notice.
    pub async fn set_source_coverage(&self, source_id: &str, coverage: f64) -> Result<()> {
        sqlx::query("UPDATE sources SET coverage = ?, updated_at = ? WHERE id = ?")
            .bind(coverage)
            .bind(now())
            .bind(source_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_sources(&self, limit: i64, offset: i64) -> Result<Vec<Source>> {
        let rows =
            sqlx::query("SELECT * FROM sources ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(row_to_source).collect())
    }

    pub async fn delete_source(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM sources WHERE id = ?")
            .bind(id)
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
    use crate::store::Store;

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_source("hello world", "web", Some("greeting"))
            .await
            .unwrap();
        assert_eq!(src.status, SourceStatus::Raw);
        assert_eq!(src.content_hash, content_hash("hello world"));

        let got = s.get_source(&src.id).await.unwrap();
        assert_eq!(got.raw_text, "hello world");
        assert_eq!(got.title_hint.as_deref(), Some("greeting"));
    }

    #[tokio::test]
    async fn find_by_hash_detects_duplicate_text() {
        let s = Store::memory().await.unwrap();
        let a = s.insert_source("same text", "web", None).await.unwrap();
        let found = s.find_by_hash(&content_hash("same text")).await.unwrap();
        assert_eq!(found.unwrap().id, a.id);
        assert!(
            s.find_by_hash(&content_hash("other"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn status_transitions_persist() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_source("x", "web", None).await.unwrap();
        s.set_source_status(&src.id, SourceStatus::Ready)
            .await
            .unwrap();
        assert_eq!(
            s.get_source(&src.id).await.unwrap().status,
            SourceStatus::Ready
        );
    }

    #[tokio::test]
    async fn get_missing_source_is_not_found() {
        let s = Store::memory().await.unwrap();
        assert!(matches!(
            s.get_source("nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn list_is_newest_first() {
        let s = Store::memory().await.unwrap();
        let a = s.insert_source("first", "web", None).await.unwrap();
        let b = s.insert_source("second", "web", None).await.unwrap();
        let list = s.list_sources(10, 0).await.unwrap();
        assert_eq!(list[0].id, b.id);
        assert_eq!(list[1].id, a.id);
    }
}
