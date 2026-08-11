use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::Row;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CorpusStatus {
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
    /// Set when this row is a placeholder for a corpus that was never captured
    /// here — its artifacts came back from the vector store and needed a parent
    /// to hang from. `raw_text` is then those artifacts joined, not the source
    /// document, so nothing that reasons about the original text should trust
    /// it. `None` for every ordinary capture.
    pub restored_at: Option<i64>,
}

/// A stored corpus that a new capture looks like.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NearDuplicate {
    pub corpus_id: String,
    pub title_hint: Option<String>,
    pub similarity: f64,
}

pub fn content_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
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
        restored_at: r.get("restored_at"),
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
        self.insert_corpus_with_signature(raw_text, origin, title_hint, sig)
            .await
    }

    /// Insert a capture whose shingle signature the caller already computed.
    ///
    /// Ingest needs the signature before the row exists, to ask whether this is
    /// a near-duplicate of something already stored. Handing it over rather
    /// than recomputing it here is what makes that one pass over the document
    /// instead of two.
    pub async fn insert_corpus_with_signature(
        &self,
        raw_text: &str,
        origin: &str,
        title_hint: Option<&str>,
        shingles: Vec<u64>,
    ) -> Result<Corpus> {
        let src = Corpus {
            id: new_id(),
            raw_text: raw_text.to_string(),
            origin: origin.to_string(),
            title_hint: title_hint.map(str::to_string),
            content_hash: content_hash(raw_text),
            status: CorpusStatus::Raw,
            created_at: now(),
            updated_at: now(),
            coverage: None,
            shingles,
            near_dupe_of: None,
            near_dupe_score: None,
            // A capture, not a placeholder. See `ensure_restored_corpus`.
            restored_at: None,
        };
        sqlx::query(
            "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at, shingles)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .execute(&self.pool)
        .await?;
        Ok(src)
    }

    /// Insert the placeholder parent a restored artifact needs, if it is not
    /// already there. Returns whether a row was created.
    ///
    /// `artifacts.corpus_id` is NOT NULL and references this table, so an
    /// artifact whose corpus row is gone — the whole-database-lost case this
    /// exists for — cannot be restored without one. Everything here is derived
    /// rather than invented where that is possible at all: the id is the one the
    /// vector payload named, and `raw_text` is the restored artifacts joined,
    /// which is genuinely all of that document still in the system.
    ///
    /// `content_hash` is seeded from the id rather than from `raw_text` because
    /// the column is UNIQUE and this is not a capture: two stubs whose artifacts
    /// happen to hold identical text are still two different sources, and
    /// hashing the reconstructed text would make the second insert fail. Seeding
    /// from the id also keeps a stub from ever colliding with a real capture of
    /// the same text, which would silently attach these artifacts to it.
    ///
    /// `Partial` is the honest status: some of this source is present, and how
    /// much is unknowable. `shingles` stays empty so the near-duplicate
    /// comparison skips it — the reconstructed text is not the document, and
    /// letting it be compared would report near-duplicates that do not exist.
    pub async fn ensure_restored_corpus(&self, id: &str, raw_text: &str) -> Result<bool> {
        let at = now();
        let res = sqlx::query(
            "INSERT INTO corpora (id, raw_text, origin, title_hint, content_hash, status, created_at, updated_at, shingles, restored_at)
             VALUES (?, ?, ?, NULL, ?, ?, ?, ?, '', ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(raw_text)
        .bind("restored:vector-store")
        .bind(content_hash(&format!("restored:{id}")))
        .bind(CorpusStatus::Partial.as_str())
        .bind(at)
        .bind(at)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
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
    /// the document, which nothing used to notice.
    pub async fn set_corpus_coverage(&self, corpus_id: &str, coverage: f64) -> Result<()> {
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

    pub async fn list_corpora(&self, limit: i64, offset: i64) -> Result<Vec<Corpus>> {
        let rows =
            sqlx::query("SELECT * FROM corpora ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(row_to_corpus).collect())
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
}
