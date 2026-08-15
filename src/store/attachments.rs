//! The bytes a corpus was captured from, when it was not text.
//!
//! One row per image corpus today. The original is kept exactly as uploaded —
//! that is the verbatim source, the way `raw_text` is for a paste — and the
//! preview is derived once, for the model and the screen.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: i64,
    pub corpus_id: String,
    pub kind: String,
    pub mime: String,
    pub filename: Option<String>,
    pub bytes: Vec<u8>,
    pub preview: Vec<u8>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub created_at: i64,
}

pub struct NewAttachment<'a> {
    pub corpus_id: &'a str,
    pub kind: &'a str,
    pub mime: &'a str,
    pub filename: Option<&'a str>,
    pub bytes: &'a [u8],
    pub preview: &'a [u8],
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// An attachment for a corpus that does not exist yet: `NewAttachment` minus
/// the id, for the door that writes row and attachment in one transaction.
pub struct NewImage<'a> {
    pub kind: &'a str,
    pub mime: &'a str,
    pub filename: Option<&'a str>,
    pub bytes: &'a [u8],
    pub preview: &'a [u8],
    pub width: Option<i64>,
    pub height: Option<i64>,
}

impl<'a> NewImage<'a> {
    pub fn for_corpus(&self, corpus_id: &'a str) -> NewAttachment<'a> {
        NewAttachment {
            corpus_id,
            kind: self.kind,
            mime: self.mime,
            filename: self.filename,
            bytes: self.bytes,
            preview: self.preview,
            width: self.width,
            height: self.height,
        }
    }
}

/// `insert_attachment` on whatever executor the caller is inside.
pub(crate) async fn insert_attachment_with<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    a: &NewAttachment<'_>,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO attachments (corpus_id, kind, mime, filename, bytes, preview, width, height, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(a.corpus_id)
    .bind(a.kind)
    .bind(a.mime)
    .bind(a.filename)
    .bind(a.bytes)
    .bind(a.preview)
    .bind(a.width)
    .bind(a.height)
    .bind(now())
    .execute(exec)
    .await?;
    Ok(res.last_insert_rowid())
}

/// What every preview is encoded as. See `core::image::prepare`.
pub const PREVIEW_MIME: &str = "image/jpeg";

impl Store {
    pub async fn insert_attachment(&self, a: &NewAttachment<'_>) -> Result<i64> {
        insert_attachment_with(&self.pool, a).await
    }

    pub async fn attachment_for_corpus(&self, corpus_id: &str) -> Result<Option<Attachment>> {
        let row = sqlx::query(
            "SELECT id, corpus_id, kind, mime, filename, bytes, preview, width, height, created_at
             FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1",
        )
        .bind(corpus_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Attachment {
            id: r.get("id"),
            corpus_id: r.get("corpus_id"),
            kind: r.get("kind"),
            mime: r.get("mime"),
            filename: r.get("filename"),
            bytes: r.get("bytes"),
            preview: r.get("preview"),
            width: r.get("width"),
            height: r.get("height"),
            created_at: r.get("created_at"),
        }))
    }

    /// The preview alone. Separate from `attachment_for_corpus` so serving a
    /// thumbnail does not read the original's megabytes off disk.
    pub async fn attachment_preview(&self, corpus_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row =
            sqlx::query("SELECT preview FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1")
                .bind(corpus_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| (PREVIEW_MIME.to_string(), r.get("preview"))))
    }

    pub async fn attachment_original(&self, corpus_id: &str) -> Result<Option<(String, Vec<u8>)>> {
        let row = sqlx::query(
            "SELECT mime, bytes FROM attachments WHERE corpus_id = ? ORDER BY id LIMIT 1",
        )
        .bind(corpus_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.get("mime"), r.get("bytes"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_attachment_round_trips_and_goes_with_its_corpus() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.insert_attachment(&NewAttachment {
            corpus_id: &src.id,
            kind: "image",
            mime: "image/png",
            filename: Some("x.png"),
            bytes: b"orig",
            preview: b"prev",
            width: Some(10),
            height: Some(20),
        })
        .await
        .unwrap();

        let a = s.attachment_for_corpus(&src.id).await.unwrap().unwrap();
        assert_eq!(a.bytes, b"orig");
        assert_eq!(a.preview, b"prev");
        assert_eq!(a.mime, "image/png");
        assert_eq!((a.width, a.height), (Some(10), Some(20)));
        assert_eq!(
            s.attachment_preview(&src.id).await.unwrap().unwrap(),
            (PREVIEW_MIME.to_string(), b"prev".to_vec())
        );
        assert_eq!(
            s.attachment_original(&src.id).await.unwrap().unwrap(),
            ("image/png".to_string(), b"orig".to_vec())
        );

        s.delete_corpus(&src.id).await.unwrap();
        assert!(s.attachment_for_corpus(&src.id).await.unwrap().is_none());
    }
}
