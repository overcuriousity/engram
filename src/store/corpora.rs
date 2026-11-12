use super::jobs::Stage;
use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;

/// Capture reports a duplicate rather than queueing synthesis a second time.
#[derive(Debug, Clone)]
pub enum Insertion {
    Created(Corpus),
    /// The same bytes were already stored; this is the row that won.
    Existing(Corpus),
}

impl Insertion {
    pub fn into_corpus(self) -> Corpus {
        match self {
            Insertion::Created(c) | Insertion::Existing(c) => c,
        }
    }
}

/// One value, because status and unit are never chosen independently: a mismatch would
/// leave the corpus in flight for ever.
// Not `Copy`: `CorpusStatus` is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub status: CorpusStatus,
    pub stage: Stage,
}

impl Reading {
    pub const VISION: Reading = Reading {
        status: CorpusStatus::Describing,
        stage: Stage::Describe,
    };
    /// No model, so no role gates it.
    pub const EXTRACTION: Reading = Reading {
        status: CorpusStatus::Extracting,
        stage: Stage::Extract,
    };
}

/// Deserialised too, so clients match on the variant: `as_str` writes `needs_review` for
/// the database while serde writes `needsreview`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CorpusStatus {
    Describing,
    Extracting,
    Raw,
    /// Not queued for synthesis: something near-identical is already in the base. Resolved on Ops.
    NeedsReview,
    Segmenting,
    Segmented,
    Embedding,
    Ready,
    Partial,
    Failed,
}

impl CorpusStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CorpusStatus::Describing => "describing",
            CorpusStatus::Extracting => "extracting",
            CorpusStatus::Raw => "raw",
            CorpusStatus::NeedsReview => "needs_review",
            CorpusStatus::Segmenting => "segmenting",
            CorpusStatus::Segmented => "segmented",
            CorpusStatus::Embedding => "embedding",
            CorpusStatus::Ready => "ready",
            CorpusStatus::Partial => "partial",
            CorpusStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> CorpusStatus {
        match s {
            "describing" => CorpusStatus::Describing,
            "extracting" => CorpusStatus::Extracting,
            "needs_review" => CorpusStatus::NeedsReview,
            "segmenting" => CorpusStatus::Segmenting,
            "segmented" => CorpusStatus::Segmented,
            "embedding" => CorpusStatus::Embedding,
            "ready" => CorpusStatus::Ready,
            "partial" => CorpusStatus::Partial,
            "failed" => CorpusStatus::Failed,
            _ => CorpusStatus::Raw,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CorpusOrigin {
    pub id: String,
    pub title_hint: Option<String>,
    pub origin: String,
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Corpus {
    pub id: String,
    pub raw_text: String,
    pub origin: String,
    pub title_hint: Option<String>,
    pub content_hash: String,
    pub status: CorpusStatus,
    pub created_at: i64,
    pub updated_at: i64,
    /// `None` for sources segmented before the check existed.
    pub coverage: Option<f64>,
    /// Empty for corpora captured before the signature existed, which are not compared.
    #[serde(skip)]
    pub shingles: Vec<u64>,
    /// Both cleared when an operator chooses to keep both.
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
    pub source_url: Option<String>,
    /// Set on a placeholder for artifacts restored from the vector store: `raw_text` is then
    /// those artifacts joined, not the source document.
    pub restored_at: Option<i64>,
    /// Set once, before the read, so a failing read is not retried every sweep.
    #[serde(skip)]
    pub auto_reread_at: Option<i64>,
    /// Namespaced JSON; `{}` when nothing was recorded.
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CorpusSummary {
    pub id: String,
    /// The first 400 characters of `raw_text`, to label a row with no title.
    pub opening: String,
    pub origin: String,
    pub title_hint: Option<String>,
    pub status: CorpusStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub coverage: Option<f64>,
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
    pub source_url: Option<String>,
    pub restored_at: Option<i64>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NearDuplicate {
    pub corpus_id: String,
    pub title_hint: Option<String>,
    pub similarity: f64,
}

#[derive(Debug, Clone)]
pub enum Followup {
    Nothing,
    Queue(Stage),
    /// Never a wait: see `Core::park_or_queue`.
    Flag {
        of: String,
        similarity: f64,
    },
}

impl Followup {
    /// A flagged capture is read like any other: the flag is a note for a person, not a gate.
    fn stage(&self) -> Option<Stage> {
        match self {
            Followup::Nothing => None,
            Followup::Queue(stage) => Some(*stage),
            Followup::Flag { .. } => Some(Stage::Synthesize),
        }
    }
}

/// `ON CONFLICT(content_hash) DO NOTHING`; answers whether the row was written.
async fn insert_corpus_row<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    src: &Corpus,
) -> Result<bool> {
    let res = sqlx::query!(
        "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at,
                              shingles, source_url, metadata, near_dupe_of, near_dupe_score)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(content_hash) DO NOTHING",
        &src.id,
        &src.raw_text,
        &src.origin,
        &src.title_hint,
        &src.content_hash,
        src.status.as_str(),
        src.created_at,
        src.updated_at,
        super::shingle::encode(&src.shingles),
        &src.source_url,
        src.metadata.to_string(),
        &src.near_dupe_of,
        src.near_dupe_score
    )
        .execute(exec)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub fn content_hash(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

#[derive(sqlx::FromRow)]
pub(crate) struct CorpusRow {
    pub id: Option<String>,
    pub raw_text: String,
    pub origin: String,
    pub title_hint: Option<String>,
    pub content_hash: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub coverage: Option<f64>,
    pub shingles: Option<String>,
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
    pub source_url: Option<String>,
    pub restored_at: Option<i64>,
    #[allow(dead_code, reason = "filled by query_as!, unread here")]
    pub retired_at: Option<i64>,
    pub auto_reread_at: Option<i64>,
    pub metadata: String,
}

fn row_to_corpus(r: CorpusRow) -> Corpus {
    Corpus {
        id: r.id.unwrap_or_default(),
        raw_text: r.raw_text,
        origin: r.origin,
        title_hint: r.title_hint,
        content_hash: r.content_hash,
        status: CorpusStatus::parse(r.status.as_str()),
        created_at: r.created_at,
        updated_at: r.updated_at,
        coverage: r.coverage,
        shingles: r
            .shingles
            .map(|s| super::shingle::decode(&s))
            .unwrap_or_default(),
        near_dupe_of: r.near_dupe_of,
        near_dupe_score: r.near_dupe_score,
        source_url: r.source_url,
        restored_at: r.restored_at,
        auto_reread_at: r.auto_reread_at,
        metadata: serde_json::from_str(&r.metadata).unwrap_or_else(|_| serde_json::json!({})),
    }
}

impl Store {
    pub async fn insert_corpus(
        &self,
        raw_text: &str,
        origin: &str,
        title_hint: Option<&str>,
    ) -> Result<Corpus> {
        let sig = super::shingle::signature(raw_text);
        Ok(self
            .insert_corpus_with_signature(
                raw_text,
                origin,
                title_hint,
                sig,
                None,
                &serde_json::json!({}),
                Followup::Nothing,
            )
            .await?
            .into_corpus())
    }

    /// The caller checks `content_hash` before a slow near-duplicate scan, so two identical
    /// captures can race: the loser gets the winner's row. The follow-up shares the transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_corpus_with_signature(
        &self,
        raw_text: &str,
        origin: &str,
        title_hint: Option<&str>,
        shingles: Vec<u64>,
        source_url: Option<&str>,
        metadata: &serde_json::Value,
        followup: Followup,
    ) -> Result<Insertion> {
        let (status, near_dupe_of, near_dupe_score) = match &followup {
            Followup::Flag { of, similarity } => {
                (CorpusStatus::Raw, Some(of.clone()), Some(*similarity))
            }
            Followup::Queue(_) | Followup::Nothing => (CorpusStatus::Raw, None, None),
        };
        let src = Corpus {
            id: new_id(),
            raw_text: raw_text.to_string(),
            origin: origin.to_string(),
            title_hint: title_hint.map(str::to_string),
            content_hash: content_hash(raw_text),
            status,
            created_at: now(),
            updated_at: now(),
            coverage: None,
            shingles,
            near_dupe_of,
            near_dupe_score,
            source_url: source_url.map(str::to_string),
            restored_at: None,
            auto_reread_at: None,
            metadata: metadata.clone(),
        };
        let mut tx = self.pool.begin().await?;
        if !insert_corpus_row(&mut *tx, &src).await? {
            tx.rollback().await?;
            let existing = self.find_by_hash(&src.content_hash).await?.ok_or_else(|| {
                // Only reachable if the winning row was deleted between the conflict and this read.
                Error::Store("capture conflicted with a corpus that then vanished".into())
            })?;
            return Ok(Insertion::Existing(existing));
        }
        tx.commit().await?;
        // After the commit: the queue is another database, and a worker claiming a unit for an
        // invisible corpus closes it as deleted. See `jobs::enqueue_with`.
        if let Some(stage) = followup.stage() {
            super::jobs::enqueue_with(&self.control, &self.subject, stage, "corpus", &src.id)
                .await?;
        }
        Ok(Insertion::Created(src))
    }

    /// Four columns, not `SELECT *`: `raw_text` can be a whole book. `Ok(None)` for a gone
    /// corpus: a reading is not refused because its document was deleted.
    pub async fn corpus_origin(&self, id: &str) -> Result<Option<CorpusOrigin>> {
        let row = sqlx::query!(
            r#"SELECT id AS "id!", title_hint, origin, source_url FROM corpora WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| CorpusOrigin {
            id: r.id,
            title_hint: r.title_hint,
            origin: r.origin,
            source_url: r.source_url,
        }))
    }

    /// Lets byte routes answer a revalidation before reading the blob.
    pub async fn content_hash_of(&self, id: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT content_hash FROM corpora WHERE id = ?", id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn corpus_titles(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let holes = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, title_hint FROM corpora WHERE title_hint IS NOT NULL AND id IN ({holes})"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("title_hint")))
            .collect())
    }

    pub async fn get_corpus(&self, id: &str) -> Result<Corpus> {
        let row = sqlx::query_as!(
            crate::store::corpora::CorpusRow,
            "SELECT * FROM corpora WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound)?;
        Ok(row_to_corpus(row))
    }

    pub async fn find_by_hash(&self, hash: &str) -> Result<Option<Corpus>> {
        let row = sqlx::query_as!(
            crate::store::corpora::CorpusRow,
            "SELECT * FROM corpora WHERE content_hash = ?",
            hash
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_corpus))
    }

    /// Status commits first, then the unit is armed: the two databases share no transaction, and
    /// a corpus with no unit is what `jobs/reconcile.rs` recovers.
    pub async fn apply_followup(&self, corpus_id: &str, followup: Followup) -> Result<()> {
        let (status, near_dupe_of, near_dupe_score) = match &followup {
            Followup::Flag { of, similarity } => {
                (CorpusStatus::Raw, Some(of.clone()), Some(*similarity))
            }
            Followup::Queue(_) | Followup::Nothing => (CorpusStatus::Raw, None, None),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE corpora SET status = ?, near_dupe_of = ?, near_dupe_score = ?, updated_at = ?
              WHERE id = ?",
            status.as_str(),
            &near_dupe_of,
            near_dupe_score,
            now(),
            corpus_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // Commit first: see `insert_corpus_with_signature`.
        if let Some(stage) = followup.stage() {
            super::jobs::enqueue_with(&self.control, &self.subject, stage, "corpus", corpus_id)
                .await?;
        }
        Ok(())
    }

    pub async fn set_corpus_status(&self, id: &str, status: CorpusStatus) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET status = ?, updated_at = ? WHERE id = ?",
            status.as_str(),
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Capture makes no inference call, so the name arrives once synthesis has read the document.
    pub async fn set_title_hint(&self, id: &str, title: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET title_hint = ?, updated_at = ? WHERE id = ?",
            title,
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `None` forgets the last measurement; the reconcile sweep reads it as unfinished.
    pub async fn set_corpus_coverage(&self, corpus_id: &str, coverage: Option<f64>) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET coverage = ?, updated_at = ? WHERE id = ?",
            coverage,
            now(),
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A full scan: hundreds of corpora at a few KB each is milliseconds; MinHash band indexes
    /// are for a scale this design does not target.
    pub async fn find_near_duplicate(
        &self,
        sig: &[u64],
        min: f64,
    ) -> Result<Option<NearDuplicate>> {
        if sig.is_empty() {
            return Ok(None);
        }
        let rows =
            sqlx::query!(r#"SELECT id AS "id!", title_hint, shingles AS "shingles!" FROM corpora WHERE shingles IS NOT NULL"#)
                .fetch_all(&self.pool)
                .await?;

        let mut best: Option<NearDuplicate> = None;
        for r in rows {
            let stored: String = r.shingles;
            let s = super::shingle::similarity(sig, &super::shingle::decode(&stored));
            if s < min {
                continue;
            }
            if best.as_ref().is_none_or(|b| s > b.similarity) {
                best = Some(NearDuplicate {
                    corpus_id: r.id,
                    title_hint: r.title_hint,
                    similarity: s,
                });
            }
        }
        Ok(best)
    }

    pub async fn set_near_dupe(
        &self,
        corpus_id: &str,
        of: Option<&str>,
        score: Option<f64>,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET near_dupe_of = ?, near_dupe_score = ?, updated_at = ? WHERE id = ?",
            of,
            score,
            now(),
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_auto_reread(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET auto_reread_at = ? WHERE id = ?",
            now(),
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Not a delete and not a hide: see `schema.sql`.
    pub async fn retire_corpus(&self, corpus_id: &str, at: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET retired_at = ? WHERE id = ?",
            at,
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn unretire_corpus(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET retired_at = NULL WHERE id = ?",
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn is_retired(&self, corpus_id: &str) -> Result<bool> {
        let at: Option<Option<i64>> =
            sqlx::query_scalar!("SELECT retired_at FROM corpora WHERE id = ?", corpus_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(matches!(at, Some(Some(_))))
    }

    pub async fn retired_among(
        &self,
        corpus_ids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        if corpus_ids.is_empty() {
            return Ok(Default::default());
        }
        let marks = std::iter::repeat_n("?", corpus_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        // Only a counted run of `?` is spliced; values are bound.
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM corpora WHERE retired_at IS NOT NULL AND id IN ({marks})"
        )));
        for id in corpus_ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool).await?.into_iter().collect())
    }

    /// Retired notes included: a done reminder still answered the query it was typed from.
    /// The link is `metadata.search.event_id`; see `core::ingest::typed_from`.
    pub async fn captures_typed_from(&self, event_ids: &[String]) -> Result<Vec<(String, String)>> {
        if event_ids.is_empty() {
            return Ok(vec![]);
        }
        let marks = std::iter::repeat_n("?", event_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        // Only a counted run of `?` is spliced; values are bound.
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(format!(
            "SELECT id, json_extract(metadata, '$.search.event_id') AS event_id
               FROM corpora
              WHERE json_extract(metadata, '$.search.event_id') IN ({marks})
              ORDER BY created_at, id"
        )));
        for id in event_ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// Avoids `SELECT *` (the idle rail renders on every box-clear). `substr` counts characters.
    /// Retired notes are excluded.
    pub async fn recent_captures(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, Option<String>, String, i64, String)>> {
        Ok(sqlx::query!(
            r#"SELECT id AS "id!", title_hint, origin, created_at, substr(raw_text, 1, 400) AS "opening!: String"
             FROM corpora WHERE retired_at IS NULL
             ORDER BY created_at DESC, id DESC LIMIT ?"#,
            limit
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.id, r.title_hint, r.origin, r.created_at, r.opening))
        .collect())
    }

    pub async fn list_corpora(&self, limit: i64, offset: i64) -> Result<Vec<Corpus>> {
        let rows = sqlx::query_as!(
            crate::store::corpora::CorpusRow,
            "SELECT * FROM corpora ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_corpus).collect())
    }

    /// Never selects `raw_text`. A keyset: the list grows at its head (`web::page::Cursor`).
    pub async fn list_corpus_summaries(
        &self,
        before: Option<&(i64, String)>,
        limit: i64,
    ) -> Result<Vec<CorpusSummary>> {
        let (ts, id) = match before {
            Some((ts, id)) => (*ts, id.as_str()),
            None => (i64::MAX, ""),
        };
        let rows = sqlx::query!(
            r#"SELECT id AS "id!", substr(raw_text, 1, 400) AS "opening!", origin AS "origin!", title_hint, status, created_at, updated_at, coverage, near_dupe_of, near_dupe_score, source_url, restored_at, metadata
               FROM corpora
              WHERE created_at < ? OR (created_at = ? AND id < ?)
              ORDER BY created_at DESC, id DESC LIMIT ?"#,
            ts,
            ts,
            id,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| CorpusSummary {
                id: r.id,
                opening: r.opening,
                origin: r.origin,
                title_hint: r.title_hint,
                status: CorpusStatus::parse(r.status.as_str()),
                created_at: r.created_at,
                updated_at: r.updated_at,
                coverage: r.coverage,
                near_dupe_of: r.near_dupe_of,
                near_dupe_score: r.near_dupe_score,
                source_url: r.source_url,
                restored_at: r.restored_at,
                metadata: serde_json::from_str(&r.metadata)
                    .unwrap_or_else(|_| serde_json::json!({})),
            })
            .collect())
    }

    pub async fn corpora_between(&self, from: i64, to: i64) -> Result<Vec<Corpus>> {
        let rows = sqlx::query_as!(
            crate::store::corpora::CorpusRow,
            "SELECT * FROM corpora WHERE created_at >= ? AND created_at < ? ORDER BY created_at, id",
            from,
            to
        ).fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_corpus).collect())
    }

    /// Newest-first offset paging skips a corpus per mid-sweep insert; the (created_at, id)
    /// cursor is stable, and unique because ids are uuid v7.
    pub async fn list_corpora_after(
        &self,
        after: Option<&(i64, String)>,
        limit: i64,
    ) -> Result<Vec<Corpus>> {
        let (ts, id) = match after {
            Some((ts, id)) => (*ts, id.as_str()),
            None => (i64::MIN, ""),
        };
        let rows = sqlx::query_as!(
            crate::store::corpora::CorpusRow,
            "SELECT * FROM corpora
             WHERE created_at > ? OR (created_at = ? AND id > ?)
             ORDER BY created_at ASC, id ASC LIMIT ?",
            ts,
            ts,
            id,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_corpus).collect())
    }

    /// The hash is the caller's, over the bytes. Row, attachment and reading unit land in one
    /// transaction, or the row lies about what it holds.
    // One argument per fact about the file, each known to some doors only.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_attached_corpus(
        &self,
        content_hash: &str,
        origin: &str,
        title_hint: Option<&str>,
        source_url: Option<&str>,
        metadata: &serde_json::Value,
        reading: Reading,
        attachment: &super::attachments::NewFile<'_>,
    ) -> Result<Insertion> {
        let at = now();
        let src = Corpus {
            id: new_id(),
            raw_text: String::new(),
            origin: origin.to_string(),
            title_hint: title_hint.map(str::to_string),
            content_hash: content_hash.to_string(),
            status: reading.status,
            created_at: at,
            updated_at: at,
            coverage: None,
            shingles: vec![],
            near_dupe_of: None,
            near_dupe_score: None,
            source_url: source_url.map(str::to_string),
            restored_at: None,
            auto_reread_at: None,
            metadata: metadata.clone(),
        };
        let mut tx = self.pool.begin().await?;
        if !insert_corpus_row(&mut *tx, &src).await? {
            tx.rollback().await?;
            let existing = self.find_by_hash(content_hash).await?.ok_or_else(|| {
                Error::Store("a file capture conflicted with a corpus that then vanished".into())
            })?;
            return Ok(Insertion::Existing(existing));
        }
        super::attachments::insert_attachment_with(&mut *tx, &attachment.for_corpus(&src.id))
            .await?;
        tx.commit().await?;
        // Commit first: the corpus and attachment must be visible before the unit is claimable.
        super::jobs::enqueue_with(
            &self.control,
            &self.subject,
            reading.stage,
            "corpus",
            &src.id,
        )
        .await?;
        Ok(Insertion::Created(src))
    }

    pub async fn clear_read_text(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE corpora SET raw_text = '', shingles = '', updated_at = ? WHERE id = ?",
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Text and signature together, so shingle comparison matches what the row says. Status is
    /// left to the caller.
    pub async fn set_read_text(&self, id: &str, text: &str, shingles: Vec<u64>) -> Result<()> {
        let res = sqlx::query!(
            "UPDATE corpora SET raw_text = ?, shingles = ?, updated_at = ? WHERE id = ?",
            text,
            super::shingle::encode(&shingles),
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn set_corpus_metadata(&self, id: &str, metadata: &serde_json::Value) -> Result<()> {
        let res = sqlx::query!(
            "UPDATE corpora SET metadata = ?, updated_at = ? WHERE id = ?",
            metadata.to_string(),
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn delete_corpus(&self, id: &str) -> Result<()> {
        let res = sqlx::query!("DELETE FROM corpora WHERE id = ?", id)
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
    async fn where_a_corpus_came_from_is_read_without_its_text() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("a very long body", "web", Some("Handbuch"))
            .await
            .unwrap();
        let got = s.corpus_origin(&src.id).await.unwrap().unwrap();
        assert_eq!(got.id, src.id);
        assert_eq!(got.title_hint.as_deref(), Some("Handbuch"));
        assert_eq!(got.origin, "web");

        assert!(s.corpus_origin("gone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_retired_corpus_leaves_the_recent_list_and_comes_back_on_undo() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("remind me friday to send the invoice", "ui", None)
            .await
            .unwrap();
        assert_eq!(s.recent_captures(5).await.unwrap().len(), 1);

        s.retire_corpus(&src.id, 1_700_000_000).await.unwrap();
        assert!(
            s.recent_captures(5).await.unwrap().is_empty(),
            "a retired note is not a recent capture"
        );
        assert!(s.is_retired(&src.id).await.unwrap());

        s.unretire_corpus(&src.id).await.unwrap();
        assert_eq!(
            s.recent_captures(5).await.unwrap().len(),
            1,
            "undo puts it back"
        );
        assert!(!s.is_retired(&src.id).await.unwrap());
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("hello world", "web", Some("greeting"))
            .await
            .unwrap();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert_eq!(src.content_hash, content_hash("hello world"));

        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.raw_text, "hello world");
        assert_eq!(got.title_hint.as_deref(), Some("greeting"));
    }

    #[tokio::test]
    async fn a_second_insert_of_the_same_bytes_yields_the_stored_row() {
        // The duplicate check and the insert are separated by a scan; losing the race must not 500.
        let s = Store::memory().await.unwrap();
        let sig = super::super::shingle::signature("the same text");
        let first = s
            .insert_corpus_with_signature(
                "the same text",
                "web",
                None,
                sig.clone(),
                None,
                &serde_json::json!({}),
                Followup::Nothing,
            )
            .await
            .unwrap();
        let second = s
            .insert_corpus_with_signature(
                "the same text",
                "mcp",
                Some("later"),
                sig,
                None,
                &serde_json::json!({}),
                Followup::Nothing,
            )
            .await
            .unwrap();

        let (Insertion::Created(a), Insertion::Existing(b)) = (first, second) else {
            panic!("the second insert did not report the first one's row");
        };
        assert_eq!(a.id, b.id);
        assert_eq!(b.origin, "web", "the loser overwrote the stored capture");
        assert_eq!(s.list_corpora(10, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_sweep_page_survives_a_capture_landing_between_pages() {
        // Offset paging over a newest-first list skips one corpus per insertion.
        let s = Store::memory().await.unwrap();
        let mut expected = vec![];
        for i in 0..6 {
            expected.push(
                s.insert_corpus(&format!("text {i}"), "web", None)
                    .await
                    .unwrap()
                    .id,
            );
        }

        let mut seen = vec![];
        let mut cursor: Option<(i64, String)> = None;
        loop {
            let page = s.list_corpora_after(cursor.as_ref(), 3).await.unwrap();
            let Some(last) = page.last() else { break };
            cursor = Some((last.created_at, last.id.clone()));
            seen.extend(page.iter().map(|c| c.id.clone()));
            if seen.len() == 3 {
                s.insert_corpus("a capture mid-sweep", "web", None)
                    .await
                    .unwrap();
            }
        }

        for id in &expected {
            assert!(seen.contains(id), "corpus {id} was stepped over: {seen:?}");
        }
    }

    #[tokio::test]
    async fn a_title_can_be_written_after_the_fact() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("some text", "web", None).await.unwrap();
        assert!(src.title_hint.is_none());

        s.set_title_hint(&src.id, "Unattended Upgrades on Debian")
            .await
            .unwrap();

        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(
            got.title_hint.as_deref(),
            Some("Unattended Upgrades on Debian")
        );
    }

    #[test]
    fn content_hash_is_one_function_for_text_and_bytes() {
        assert_eq!(content_hash("abc"), content_hash(b"abc"));
        assert_eq!(content_hash("abc"), hex::encode(Sha256::digest(b"abc")));
    }

    #[tokio::test]
    async fn find_by_hash_detects_duplicate_text() {
        let s = Store::memory().await.unwrap();
        let a = s.insert_corpus("same text", "web", None).await.unwrap();
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
    async fn a_followup_writes_the_status_and_the_job_it_implies_together() {
        // Two statements left a window where the corpus was `raw` with nothing queued.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "image", None).await.unwrap();
        s.set_corpus_status(&src.id, CorpusStatus::Describing)
            .await
            .unwrap();

        s.apply_followup(&src.id, Followup::Queue(Stage::Synthesize))
            .await
            .unwrap();

        assert_eq!(
            s.get_corpus(&src.id).await.unwrap().status,
            CorpusStatus::Raw
        );
        assert!(
            s.live_job(Stage::Synthesize, &src.id).await.unwrap(),
            "a raw corpus with no job is a corpus nothing will pick up"
        );
    }

    #[tokio::test]
    async fn parking_a_followup_names_what_it_parked_beside_and_queues_nothing() {
        let s = Store::memory().await.unwrap();
        let other = s.insert_corpus("older", "web", None).await.unwrap();
        let src = s.insert_corpus("x", "image", None).await.unwrap();

        s.apply_followup(
            &src.id,
            Followup::Flag {
                of: other.id.clone(),
                similarity: 0.94,
            },
        )
        .await
        .unwrap();

        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::Raw);
        assert_eq!(got.near_dupe_of.as_deref(), Some(other.id.as_str()));
        assert_eq!(got.near_dupe_score, Some(0.94));
        assert!(
            s.live_job(Stage::Synthesize, &src.id).await.unwrap(),
            "a flag is not a gate"
        );
    }

    #[tokio::test]
    async fn status_transitions_persist() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        s.set_corpus_status(&src.id, CorpusStatus::Ready)
            .await
            .unwrap();
        assert_eq!(
            s.get_corpus(&src.id).await.unwrap().status,
            CorpusStatus::Ready
        );
    }

    #[tokio::test]
    async fn get_missing_source_is_not_found() {
        let s = Store::memory().await.unwrap();
        assert!(matches!(
            s.get_corpus("nope").await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn list_is_newest_first() {
        let s = Store::memory().await.unwrap();
        let a = s.insert_corpus("first", "web", None).await.unwrap();
        let b = s.insert_corpus("second", "web", None).await.unwrap();
        let list = s.list_corpora(10, 0).await.unwrap();
        assert_eq!(list[0].id, b.id);
        assert_eq!(list[1].id, a.id);
    }

    #[tokio::test]
    async fn the_consolidation_schema_is_present() {
        let s = Store::memory().await.unwrap();
        for sql in [
            "SELECT shingles, near_dupe_of, near_dupe_score FROM corpora LIMIT 1",
            "SELECT superseded_by, caveats FROM artifacts LIMIT 1",
            "SELECT id, a_id, b_id, score, state, detail, created_at FROM artifact_pairs LIMIT 1",
        ] {
            // runtime: checks what `migrate` built in memory, not the reference schema.
            sqlx::query(sql)
                .fetch_optional(&s.pool)
                .await
                .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
        }
    }

    #[tokio::test]
    async fn a_pair_is_recorded_once_whichever_order_it_is_found_in() {
        // Without a canonical order the review queue gets the same pair twice.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "one".into(),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();

        for _ in 0..2 {
            sqlx::query!(
                "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
                 VALUES (?, ?, 0.9, 'pending', 0)",
                &made[0].id,
                &made[1].id
            )
            .execute(&s.pool)
            .await
            .unwrap();
        }

        let n: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM artifact_pairs")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "the unique constraint on (a_id, b_id) is missing");
    }

    #[tokio::test]
    async fn a_stored_corpus_carries_its_signature() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("a document about mounting filesystems", "web", None)
            .await
            .unwrap();
        assert!(!src.shingles.is_empty());
        assert_eq!(s.get_corpus(&src.id).await.unwrap().shingles, src.shingles);
    }

    #[tokio::test]
    async fn a_near_identical_corpus_is_found_by_signature() {
        let s = Store::memory().await.unwrap();
        let body: String = (0..200)
            .map(|i| format!("step {i}: run the command and read the output"))
            .collect::<Vec<_>>()
            .join("\n");
        let first = s.insert_corpus(&body, "web", Some("manual")).await.unwrap();

        let edited = body.replacen("step 7", "step seven", 1);
        let hit = s
            .find_near_duplicate(&crate::store::shingle::signature(&edited), 0.90)
            .await
            .unwrap()
            .expect("the edited copy should have matched");
        assert_eq!(hit.corpus_id, first.id);
        assert_eq!(hit.title_hint.as_deref(), Some("manual"));
        assert!(hit.similarity > 0.90);
    }

    #[tokio::test]
    async fn an_unrelated_corpus_is_not_a_near_duplicate() {
        let s = Store::memory().await.unwrap();
        s.insert_corpus("a chapter about filesystems and mounting", "web", None)
            .await
            .unwrap();
        let other = crate::store::shingle::signature("a recipe for shortcrust pastry and jam");
        assert!(s.find_near_duplicate(&other, 0.90).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn needs_review_survives_a_round_trip() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        s.set_corpus_status(&src.id, CorpusStatus::NeedsReview)
            .await
            .unwrap();
        s.set_near_dupe(&src.id, Some("other-id"), Some(0.94))
            .await
            .unwrap();
        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::NeedsReview);
        assert_eq!(got.near_dupe_of.as_deref(), Some("other-id"));
        assert!((got.near_dupe_score.unwrap() - 0.94).abs() < 1e-9);

        s.set_near_dupe(&src.id, None, None).await.unwrap();
        assert!(s.get_corpus(&src.id).await.unwrap().near_dupe_of.is_none());
    }

    fn a_test_image() -> super::super::attachments::NewFile<'static> {
        super::super::attachments::NewFile {
            kind: "image",
            mime: "image/png",
            filename: Some("x.png"),
            bytes: b"orig",
            preview: b"prev",
            width: Some(10),
            height: Some(20),
        }
    }

    #[tokio::test]
    async fn a_text_capture_and_its_job_land_together() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus_with_signature(
                "hello",
                "web",
                None,
                vec![],
                None,
                &serde_json::json!({}),
                Followup::Queue(Stage::Synthesize),
            )
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert!(s.live_job(Stage::Synthesize, &src.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_flagged_capture_is_written_flagged_and_queued() {
        let s = Store::memory().await.unwrap();
        let other = s.insert_corpus("other", "web", None).await.unwrap();
        let src = s
            .insert_corpus_with_signature(
                "hello",
                "web",
                None,
                vec![],
                None,
                &serde_json::json!({}),
                Followup::Flag {
                    of: other.id.clone(),
                    similarity: 0.9,
                },
            )
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(src.status, CorpusStatus::Raw);
        assert_eq!(src.near_dupe_of.as_deref(), Some(other.id.as_str()));
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.status, CorpusStatus::Raw);
        assert_eq!(back.near_dupe_score, Some(0.9));
        assert!(s.live_job(Stage::Synthesize, &src.id).await.unwrap());
    }

    #[tokio::test]
    async fn an_image_row_its_attachment_and_its_job_land_together() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_attached_corpus(
                "hash-1",
                "image",
                None,
                None,
                &serde_json::json!({}),
                crate::store::corpora::Reading::VISION,
                &a_test_image(),
            )
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert!(s.attachment_for_corpus(&src.id).await.unwrap().is_some());
        assert!(s.live_job(Stage::Describe, &src.id).await.unwrap());
        assert!(matches!(
            s.insert_attached_corpus("hash-1", "image", None, None, &serde_json::json!({}), Reading::VISION, &a_test_image())
                .await
                .unwrap(),
            Insertion::Existing(e) if e.id == src.id
        ));
        let n: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM attachments")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn an_image_corpus_starts_describing_with_no_text_and_its_metadata() {
        let s = Store::memory().await.unwrap();
        let meta = serde_json::json!({"file": {"name": "a.jpg"}, "note": "whiteboard"});
        let ins = s
            .insert_attached_corpus(
                "hash-1",
                "image",
                Some("a.jpg"),
                None,
                &meta,
                crate::store::corpora::Reading::VISION,
                &a_test_image(),
            )
            .await
            .unwrap();
        let src = ins.into_corpus();
        assert_eq!(src.status, CorpusStatus::Describing);
        assert_eq!(src.raw_text, "");
        assert_eq!(src.content_hash, "hash-1");
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.metadata["note"], "whiteboard");
        assert_eq!(back.status, CorpusStatus::Describing);

        assert!(matches!(
            s.insert_attached_corpus("hash-1", "image", None, None, &meta, Reading::VISION, &a_test_image())
                .await
                .unwrap(),
            Insertion::Existing(e) if e.id == src.id
        ));
    }

    #[tokio::test]
    async fn describing_writes_the_text_and_signature_but_keeps_the_hash() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_attached_corpus(
                "hash-2",
                "image",
                None,
                None,
                &serde_json::json!({}),
                crate::store::corpora::Reading::VISION,
                &a_test_image(),
            )
            .await
            .unwrap()
            .into_corpus();
        let sig = crate::store::shingle::signature("hello world");
        s.set_read_text(&src.id, "hello world", sig.clone())
            .await
            .unwrap();
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.raw_text, "hello world");
        assert_eq!(back.shingles, sig);
        assert_eq!(back.content_hash, "hash-2");
        // Status is the caller's decision, not this write's.
        assert_eq!(back.status, CorpusStatus::Describing);
    }

    #[tokio::test]
    async fn metadata_defaults_to_an_empty_object_and_can_be_replaced() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("plain", "web", None).await.unwrap();
        assert_eq!(src.metadata, serde_json::json!({}));
        s.set_corpus_metadata(&src.id, &serde_json::json!({"note": "n"}))
            .await
            .unwrap();
        assert_eq!(s.get_corpus(&src.id).await.unwrap().metadata["note"], "n");
    }
}
