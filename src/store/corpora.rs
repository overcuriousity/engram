use super::jobs::Stage;
use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;

/// Which side of the `content_hash` constraint an insert came down on.
///
/// The distinction is the caller's to act on: capture reports a duplicate
/// rather than creating a second source, and must not queue synthesis for a
/// corpus somebody else already queued.
#[derive(Debug, Clone)]
pub enum Insertion {
    /// The row this call wrote.
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

/// How a capture that arrived as bytes waits to be read: the status the row
/// sits in, and the unit that will move it on.
///
/// One value rather than two arguments, because they are never chosen
/// independently. A row parked in `extracting` with a `Describe` unit behind it
/// would wait for a stage that skips it, and the corpus would sit in flight for
/// ever with nothing wrong that any one line would show.
// Not `Copy`: `CorpusStatus` is not, and making a fieldless enum copyable to
// suit one struct is a change to every place that holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub status: CorpusStatus,
    pub stage: Stage,
}

impl Reading {
    /// A photo, read by the vision model.
    pub const VISION: Reading = Reading {
        status: CorpusStatus::Describing,
        stage: Stage::Describe,
    };
    /// A PDF, read by the local extractor. No model, so no role gates it.
    pub const EXTRACTION: Reading = Reading {
        status: CorpusStatus::Extracting,
        stage: Stage::Extract,
    };
}

/// Deserialised as well as serialised, so a client can match on the variant
/// rather than on the spelling. The two spellings in this file do not agree —
/// `as_str` writes `needs_review` for the database and serde writes
/// `needsreview` for a response — and a client comparing strings would pick the
/// wrong one and silently never match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CorpusStatus {
    /// An image whose text has not been read yet. Only image corpora hold it.
    Describing,
    /// A PDF whose text has not been extracted yet. Only PDF corpora hold it.
    Extracting,
    Raw,
    /// Captured, stored, and deliberately not queued for synthesis: something
    /// near-identical is already in the base, and segmenting it would pay a
    /// model to produce artifacts that compete with ones that already exist.
    /// An operator resolves it on Ops.
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

/// The four columns that say where a corpus came from. See `corpus_origin`.
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
    /// Fraction of this source's non-blank lines that ended up inside some
    /// chunk. `None` for sources segmented before the check existed.
    pub coverage: Option<f64>,
    /// Bottom-k shingle hashes of `raw_text`. Empty for corpora captured before
    /// the signature existed, which simply are not compared.
    #[serde(skip)]
    pub shingles: Vec<u64>,
    /// The corpus this one looked like at capture, and how alike they were.
    /// Both cleared when an operator chooses to keep both.
    pub near_dupe_of: Option<String>,
    pub near_dupe_score: Option<f64>,
    /// The page this was captured from, for the two doors that know one.
    /// `None` for a paste, an upload or an MCP capture.
    pub source_url: Option<String>,
    /// Set when this row is a placeholder for a corpus that was never captured
    /// here — its artifacts came back from the vector store and needed a parent
    /// to hang from. `raw_text` is then those artifacts joined, not the source
    /// document, so nothing that reasons about the original text should trust
    /// it. `None` for every ordinary capture.
    pub restored_at: Option<i64>,
    /// What the door knew about the capture beyond the text: a `note`, `file`
    /// facts, `exif`. Namespaced JSON; `{}` when nothing was recorded.
    pub metadata: serde_json::Value,
}

/// A stored corpus that a new capture looks like.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NearDuplicate {
    pub corpus_id: String,
    pub title_hint: Option<String>,
    pub similarity: f64,
}

/// The dedupe key of a corpus: text for a capture, the file's bytes for an
/// image. One function, so the two doors can never drift onto different keys
/// for the same column.
/// What a capture wants done in the same transaction as its row.
#[derive(Debug, Clone)]
pub enum Followup {
    /// Just the row. Tests and tools that drive the queue themselves.
    Nothing,
    /// Queue this stage. Text captures queue Synthesize; images queue Describe.
    Queue(Stage),
    /// Park beside a near-duplicate: `near_dupe_of`, its score, and
    /// `needs_review`, written on the row itself. No job.
    Park { of: String, similarity: f64 },
}

/// The one INSERT every captured corpus row goes through, on whatever executor
/// the caller is inside. `ON CONFLICT(content_hash) DO NOTHING`; answers
/// whether the row was written.
async fn insert_corpus_row<'e>(
    exec: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    src: &Corpus,
) -> Result<bool> {
    let res = sqlx::query(
        "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at,
                              shingles, source_url, metadata, near_dupe_of, near_dupe_score)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(content_hash) DO NOTHING",
    )
    .bind(&src.id)
    .bind(&src.raw_text)
    .bind(&src.origin)
    .bind(&src.title_hint)
    .bind(&src.content_hash)
    .bind(src.status.as_str())
    .bind(src.created_at)
    .bind(src.updated_at)
    .bind(super::shingle::encode(&src.shingles))
    .bind(&src.source_url)
    .bind(src.metadata.to_string())
    .bind(&src.near_dupe_of)
    .bind(src.near_dupe_score)
    .execute(exec)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub fn content_hash(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

fn row_to_corpus(r: &sqlx::sqlite::SqliteRow) -> Corpus {
    Corpus {
        id: r.get("id"),
        raw_text: r.get("raw_text"),
        origin: r.get("origin"),
        title_hint: r.get("title_hint"),
        content_hash: r.get("content_hash"),
        status: CorpusStatus::parse(r.get::<String, _>("status").as_str()),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        coverage: r.get("coverage"),
        shingles: r
            .get::<Option<String>, _>("shingles")
            .map(|s| super::shingle::decode(&s))
            .unwrap_or_default(),
        near_dupe_of: r.get("near_dupe_of"),
        near_dupe_score: r.get("near_dupe_score"),
        source_url: r.get("source_url"),
        restored_at: r.get("restored_at"),
        metadata: r
            .get::<Option<String>, _>("metadata")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({})),
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

    /// Insert a capture whose shingle signature the caller already computed.
    ///
    /// Ingest needs the signature before the row exists, to ask whether this is
    /// a near-duplicate of something already stored. Handing it over rather
    /// than recomputing it here is what makes that one pass over the document
    /// instead of two.
    ///
    /// Answers whether the row is ours because the check for an existing
    /// `content_hash` happens in the caller, before the near-duplicate scan —
    /// and that scan decodes every stored signature, so the window between the
    /// check and this insert grows with the base. Two identical captures in
    /// flight (a double-submitted form, an agent retrying) both pass the check.
    /// The loser must get the winner's row back, not a UNIQUE-constraint 500.
    ///
    /// The follow-up — the job, or the park — is written in the same
    /// transaction as the row: a process that dies between the two would
    /// otherwise leave a `raw` corpus nothing will ever pick up.
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
            Followup::Park { of, similarity } => (
                CorpusStatus::NeedsReview,
                Some(of.clone()),
                Some(*similarity),
            ),
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
            metadata: metadata.clone(),
        };
        let mut tx = self.pool.begin().await?;
        if !insert_corpus_row(&mut *tx, &src).await? {
            tx.rollback().await?;
            // Somebody else got there between the caller's hash check and this
            // statement. Their row is as good as ours would have been — same
            // bytes, by definition of the constraint that rejected us.
            let existing = self.find_by_hash(&src.content_hash).await?.ok_or_else(|| {
                // Only reachable if the winning row was deleted in the moment
                // between the conflict and this read.
                Error::Store("capture conflicted with a corpus that then vanished".into())
            })?;
            return Ok(Insertion::Existing(existing));
        }
        tx.commit().await?;
        // After the commit and never before it. The queue is a second database
        // and takes no part in this transaction, so a unit armed first is
        // claimable first: a worker that gets there inside the window reads a
        // corpus that is not visible yet, and `Error::NotFound` on a claimed
        // job means "the target was deleted" — which closes the unit for good
        // and leaves the capture at `raw` with nothing that will ever pick it
        // up. See `jobs::enqueue_with` for the failure this order leaves
        // instead, which is the recoverable one.
        if let Followup::Queue(stage) = followup {
            super::jobs::enqueue_with(&self.control, &self.subject, stage, "corpus", &src.id)
                .await?;
        }
        Ok(Insertion::Created(src))
    }

    /// Where a corpus came from, without its text.
    ///
    /// `get_corpus` is `SELECT *`, and `raw_text` on a captured book is the
    /// whole book. The single-artifact read needs four columns of it to name a
    /// source, and reading the row to get them made the cheapest door in the
    /// API load tens of megabytes per request — the cost `web::api::SourceRef`
    /// was shaped to avoid on the wire and still paid in the query.
    ///
    /// `Ok(None)` and not `NotFound` for a corpus that is gone: a reading is
    /// not refused because the document behind it was deleted.
    pub async fn corpus_origin(&self, id: &str) -> Result<Option<CorpusOrigin>> {
        let row =
            sqlx::query("SELECT id, title_hint, origin, source_url FROM corpora WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| CorpusOrigin {
            id: r.get("id"),
            title_hint: r.get("title_hint"),
            origin: r.get("origin"),
            source_url: r.get("source_url"),
        }))
    }

    /// The title hints of these corpora, by id, for the ones that have one.
    ///
    /// What a passage with no heading of its own is shown under: the note it
    /// came from is one join away and is exactly what a person would call it.
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
        let row = sqlx::query("SELECT * FROM corpora WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_corpus(&row))
    }

    pub async fn find_by_hash(&self, hash: &str) -> Result<Option<Corpus>> {
        let row = sqlx::query("SELECT * FROM corpora WHERE content_hash = ?")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_corpus))
    }

    /// Write a follow-up onto a corpus that already has a row — the same three
    /// outcomes `insert_corpus_with_signature` writes beside a new one, and in
    /// the same order: the status commits, then the unit is armed.
    ///
    /// The path here is the one an image takes. Its row is written at capture
    /// and its text only exists once the vision stage has read it, so the fork
    /// between "queue this for synthesis" and "park it beside what it looks
    /// like" happens long after the insert. The two writes were once one
    /// transaction, which the queue moving to a second database ended: SQLite
    /// promises nothing across two of them, so what is left is a choice of
    /// which way round to be interrupted. Status first is the recoverable one
    /// — a corpus with no unit is what `jobs/reconcile.rs` looks for, while a
    /// unit with no visible corpus is one a worker closes as deleted.
    pub async fn apply_followup(&self, corpus_id: &str, followup: Followup) -> Result<()> {
        let (status, near_dupe_of, near_dupe_score) = match &followup {
            Followup::Park { of, similarity } => (
                CorpusStatus::NeedsReview,
                Some(of.clone()),
                Some(*similarity),
            ),
            Followup::Queue(_) | Followup::Nothing => (CorpusStatus::Raw, None, None),
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE corpora SET status = ?, near_dupe_of = ?, near_dupe_score = ?, updated_at = ?
              WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(&near_dupe_of)
        .bind(near_dupe_score)
        .bind(now())
        .bind(corpus_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // The commit first, for the reason `insert_corpus_with_signature`
        // gives: a claim that lands before the status write is visible is a
        // unit closed against a corpus the worker cannot see.
        if let Followup::Queue(stage) = followup {
            super::jobs::enqueue_with(&self.control, &self.subject, stage, "corpus", corpus_id)
                .await?;
        }
        Ok(())
    }

    pub async fn set_corpus_status(&self, id: &str, status: CorpusStatus) -> Result<()> {
        sqlx::query("UPDATE corpora SET status = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Names a corpus after the fact. Capture makes no inference call by
    /// design, so the name arrives later — once synthesis has read the document
    /// and knows what it is about.
    /// The one write to `origin` outside insert: a capture becoming, or
    /// ceasing to be, a journal entry. A channel label, never content.
    pub async fn set_corpus_origin(&self, id: &str, origin: &str) -> Result<()> {
        let n = sqlx::query("UPDATE corpora SET origin = ?, updated_at = ? WHERE id = ?")
            .bind(origin)
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    pub async fn set_title_hint(&self, id: &str, title: &str) -> Result<()> {
        sqlx::query("UPDATE corpora SET title_hint = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// How much of this source ended up inside a chunk. Written once every
    /// window has resolved; a low number means the segmenter dropped part of
    /// the document. `None` forgets what the last run measured, so the next
    /// one is measured against its own windows — and is what the
    /// reconciliation sweep reads as "this document never finished".
    pub async fn set_corpus_coverage(&self, corpus_id: &str, coverage: Option<f64>) -> Result<()> {
        sqlx::query("UPDATE corpora SET coverage = ?, updated_at = ? WHERE id = ?")
            .bind(coverage)
            .bind(now())
            .bind(corpus_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The stored corpus most like this signature, if any clears `min`.
    ///
    /// A full scan of the signature column. A single-operator base holds
    /// hundreds of corpora, each with a signature of a couple of kilobytes, so
    /// this is a few milliseconds of memory bandwidth on a path that already
    /// writes the whole document to disk. An index over MinHash bands is the
    /// answer at a scale this design does not target.
    pub async fn find_near_duplicate(
        &self,
        sig: &[u64],
        min: f64,
    ) -> Result<Option<NearDuplicate>> {
        if sig.is_empty() {
            return Ok(None);
        }
        let rows =
            sqlx::query("SELECT id, title_hint, shingles FROM corpora WHERE shingles IS NOT NULL")
                .fetch_all(&self.pool)
                .await?;

        let mut best: Option<NearDuplicate> = None;
        for r in &rows {
            let stored: String = r.get("shingles");
            let s = super::shingle::similarity(sig, &super::shingle::decode(&stored));
            if s < min {
                continue;
            }
            if best.as_ref().is_none_or(|b| s > b.similarity) {
                best = Some(NearDuplicate {
                    corpus_id: r.get("id"),
                    title_hint: r.get("title_hint"),
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
        sqlx::query(
            "UPDATE corpora SET near_dupe_of = ?, near_dupe_score = ?, updated_at = ? WHERE id = ?",
        )
        .bind(of)
        .bind(score)
        .bind(now())
        .bind(corpus_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Captures waiting on a near-duplicate decision, newest first. They are
    /// the one corpus state nothing else advances, so Ops has to show them or
    /// they sit unprocessed with no indication why.
    pub async fn parked_corpora(&self, limit: i64) -> Result<Vec<Corpus>> {
        let rows = sqlx::query(
            "SELECT * FROM corpora WHERE near_dupe_of IS NOT NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }

    /// The newest few captures, by the columns a list row shows. This is the
    /// idle rail's read, and the idle rail renders on every box-clear:
    /// `list_corpora` is `SELECT *`, and `raw_text` there is the whole
    /// document, pulled to render a 60-character label. The prefix is chars,
    /// not bytes — SQLite's `substr` on text counts characters — and 400 of
    /// them is more than any label survives `markdown::snippet` with.
    pub async fn recent_captures(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, Option<String>, String, i64, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, title_hint, origin, created_at, substr(raw_text, 1, 400) \
             FROM corpora ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_corpora(&self, limit: i64, offset: i64) -> Result<Vec<Corpus>> {
        let rows =
            sqlx::query("SELECT * FROM corpora ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }

    /// Everything captured in a span of time, oldest first — a day, read in
    /// the viewer's zone.
    pub async fn corpora_between(&self, from: i64, to: i64) -> Result<Vec<Corpus>> {
        let rows = sqlx::query("SELECT * FROM corpora WHERE created_at >= ? AND created_at < ? ORDER BY created_at, id")
            .bind(from)
            .bind(to)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }

    /// Entries that name a day in `metadata.day`: written on the day page
    /// about a day other than the one they were written on.
    pub async fn corpora_by_day(&self, day: &str) -> Result<Vec<Corpus>> {
        let rows = sqlx::query("SELECT * FROM corpora WHERE json_extract(metadata, '$.day') = ? ORDER BY created_at, id")
            .bind(day)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }

    /// A page for a sweep that has to see every corpus exactly once: oldest
    /// first, resumed from the last row of the previous page.
    ///
    /// `list_corpora` cannot do this job. It orders newest-first, so a capture
    /// landing mid-sweep shifts every later page down by one and exactly one
    /// corpus is stepped over — by the sweep whose entire purpose is to pick up
    /// what was left unfinished. A cursor over (created_at, id) is stable under
    /// inserts and deletes alike; the pair is unique because ids are uuid v7.
    pub async fn list_corpora_after(
        &self,
        after: Option<&(i64, String)>,
        limit: i64,
    ) -> Result<Vec<Corpus>> {
        let (ts, id) = match after {
            Some((ts, id)) => (*ts, id.as_str()),
            None => (i64::MIN, ""),
        };
        let rows = sqlx::query(
            "SELECT * FROM corpora
             WHERE created_at > ? OR (created_at = ? AND id > ?)
             ORDER BY created_at ASC, id ASC LIMIT ?",
        )
        .bind(ts)
        .bind(ts)
        .bind(id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
    }

    /// The row for a capture whose source is bytes rather than text. There is
    /// no text yet — a reading stage writes it — so the hash is the caller's,
    /// over those bytes. Same conflict handling as a text capture: the same
    /// file twice is one row. Row, attachment and the reading unit land in one
    /// transaction: a capture with no attachment, or one no job will ever
    /// read, is a row that lies about what it holds.
    ///
    /// `reading` is the caller's because it is what differs between the doors.
    // Eight arguments: one per fact about the file, each of which some door
    // has and another has not. A builder here would be eight setters.
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
        // The commit first: the corpus and its attachment have to be visible
        // before anything can claim the unit that reads them.
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

    /// The reverse of `set_read_text`, for a re-read: no text and no
    /// signature, so the row is comparable to nothing until the stage that
    /// reads it speaks again.
    pub async fn clear_read_text(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE corpora SET raw_text = '', shingles = '', updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// What a reading stage made of the bytes — vision for a photo, extraction
    /// for a PDF. Text and signature together, so the row is never
    /// comparable-by-shingle to something it does not say. Status is left to
    /// the caller, who knows whether this parks or proceeds.
    pub async fn set_read_text(&self, id: &str, text: &str, shingles: Vec<u64>) -> Result<()> {
        let res = sqlx::query(
            "UPDATE corpora SET raw_text = ?, shingles = ?, updated_at = ? WHERE id = ?",
        )
        .bind(text)
        .bind(super::shingle::encode(&shingles))
        .bind(now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn set_corpus_metadata(&self, id: &str, metadata: &serde_json::Value) -> Result<()> {
        let res = sqlx::query("UPDATE corpora SET metadata = ?, updated_at = ? WHERE id = ?")
            .bind(metadata.to_string())
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn delete_corpus(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM corpora WHERE id = ?")
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

    /// Four columns and not the row: `raw_text` on a captured book is the whole
    /// book, and the single-artifact read that needs a source name was loading
    /// it on every request to print a title.
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

        // A corpus deleted since is not a refusal: the artifact is still there
        // to read, it just no longer says where it came from.
        assert!(s.corpus_origin("gone").await.unwrap().is_none());
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
        // The caller's duplicate check and this insert are two statements, and
        // between them sits a scan over every stored signature. Losing that
        // race must hand back the winner's row, not a UNIQUE-constraint error
        // dressed up as a server fault.
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
        // Offset paging over a newest-first list steps over exactly one corpus
        // per insertion, and the sweep that pages this way exists to find what
        // was left unfinished.
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
            // A capture arriving mid-sweep, once.
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
        // Capture no longer asks for a label, so the only way a corpus gets a
        // name is a write once synthesis has read the document.
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
        // An image reaches this fork after its read, long after its row. Two
        // statements left a window where the corpus was `raw` with nothing
        // queued, and nothing in the system looks for that state.
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
            Followup::Park {
                of: other.id.clone(),
                similarity: 0.94,
            },
        )
        .await
        .unwrap();

        let got = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(got.status, CorpusStatus::NeedsReview);
        assert_eq!(got.near_dupe_of.as_deref(), Some(other.id.as_str()));
        assert_eq!(got.near_dupe_score, Some(0.94));
        assert!(!s.live_job(Stage::Synthesize, &src.id).await.unwrap());
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
        // Migrations run on connect, so this failing means 0009 did not apply.
        let s = Store::memory().await.unwrap();
        for sql in [
            "SELECT shingles, near_dupe_of, near_dupe_score FROM corpora LIMIT 1",
            "SELECT superseded_by, caveats FROM artifacts LIMIT 1",
            "SELECT id, a_id, b_id, score, state, detail, created_at FROM artifact_pairs LIMIT 1",
        ] {
            sqlx::query(sql)
                .fetch_optional(&s.pool)
                .await
                .unwrap_or_else(|e| panic!("{sql} failed: {e}"));
        }
    }

    #[tokio::test]
    async fn a_pair_is_recorded_once_whichever_order_it_is_found_in() {
        // The sweep sees (a,b) on one run and (b,a) on the next. Without a
        // canonical order the review queue fills with the same pair twice.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        ordinal: 0,
                        text: "one".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();

        for _ in 0..2 {
            sqlx::query(
                "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
                 VALUES (?, ?, 0.9, 'pending', 0)",
            )
            .bind(&made[0].id)
            .bind(&made[1].id)
            .execute(&s.pool)
            .await
            .unwrap();
        }

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifact_pairs")
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

        // Clearing it is what "keep both" does, and it must actually clear.
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
    async fn a_parked_capture_is_written_parked_with_no_job() {
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
                Followup::Park {
                    of: other.id.clone(),
                    similarity: 0.9,
                },
            )
            .await
            .unwrap()
            .into_corpus();
        assert_eq!(src.status, CorpusStatus::NeedsReview);
        assert_eq!(src.near_dupe_of.as_deref(), Some(other.id.as_str()));
        let back = s.get_corpus(&src.id).await.unwrap();
        assert_eq!(back.status, CorpusStatus::NeedsReview);
        assert_eq!(back.near_dupe_score, Some(0.9));
        assert!(!s.live_job(Stage::Synthesize, &src.id).await.unwrap());
        assert_eq!(s.parked_corpora(10).await.unwrap().len(), 1);
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
        // The same hash again: Existing, and nothing new written.
        assert!(matches!(
            s.insert_attached_corpus("hash-1", "image", None, None, &serde_json::json!({}), Reading::VISION, &a_test_image())
                .await
                .unwrap(),
            Insertion::Existing(e) if e.id == src.id
        ));
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments")
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

        // The same photo again is the same row.
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
