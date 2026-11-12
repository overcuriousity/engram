//! A `VectorStore` in SQLite for an engram without Qdrant: exact scan via sqlite-vec, with Qdrant's IDF,
//! rank fusion and recency formula redone in Rust so a base ranks the same in either store.

use super::sparse::SparseVector;
use super::{
    FacetCount, Facets, LifecycleRow, Recency, SearchFilter, SearchHit, StoredLifecycle, Touch,
    VectorPayload, VectorPoint, VectorStore,
};
use crate::error::{Error, Result};
use crate::store::artifacts::ArtifactStatus;
use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;

use super::qdrant::PINNED_TAG;

/// `recency` is what a plain `search` uses; `search_weighted` names its own.
#[derive(Debug, Clone, Copy)]
pub struct Scoring {
    pub recency: Recency,
    pub pinned_boost: f32,
}

impl Scoring {
    pub fn off() -> Scoring {
        Scoring {
            recency: Recency {
                weight: 0.0,
                half_life_days: 1,
            },
            pinned_boost: 0.0,
        }
    }
}

pub struct SqliteVectors {
    pool: sqlx::SqlitePool,
    scoring: Scoring,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vec_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS vec_points (
    artifact_id      TEXT PRIMARY KEY,
    corpus_id        TEXT NOT NULL,
    category         TEXT,
    status           TEXT NOT NULL DEFAULT 'active',
    created_at       INTEGER NOT NULL,
    last_seen_at     INTEGER,
    hit_count        INTEGER,
    last_verified_at INTEGER,
    payload          TEXT NOT NULL,
    embedding        BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS vec_points_corpus ON vec_points(corpus_id);
CREATE TABLE IF NOT EXISTS vec_tags (
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    tag         TEXT NOT NULL,
    PRIMARY KEY (artifact_id, tag)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS vec_sparse (
    term        INTEGER NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    value       REAL NOT NULL,
    PRIMARY KEY (term, artifact_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS vec_sparse_artifact ON vec_sparse(artifact_id);
CREATE TABLE IF NOT EXISTS vec_ctx (
    artifact_id TEXT NOT NULL REFERENCES vec_points(artifact_id) ON DELETE CASCADE,
    n           INTEGER NOT NULL,
    embedding   BLOB NOT NULL,
    PRIMARY KEY (artifact_id, n)
) WITHOUT ROWID;
";

/// An auto-extension, not `load_extension`: nothing is read from storage, so a hardened Android may forbid that.
// FFI: sqlite-vec registers through sqlite3_auto_extension.
#[allow(unsafe_code)]
fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libsqlite3_sys::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut libsqlite3_sys::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const libsqlite3_sys::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn unblob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

fn status_word(s: ArtifactStatus) -> &'static str {
    match s {
        ArtifactStatus::Active => "active",
        ArtifactStatus::Deprecated => "deprecated",
        ArtifactStatus::Superseded => "superseded",
        ArtifactStatus::Trashed => "trashed",
    }
}

fn decode(payload: &str) -> Result<VectorPayload> {
    serde_json::from_str(payload).map_err(|e| Error::Vector(format!("stored payload: {e}")))
}

fn encode(p: &VectorPayload) -> String {
    // The JSON is the whole truth here; an absent key reads back as None.
    serde_json::to_string(p).unwrap_or_default()
}

/// sqlite-vec answers NULL or NaN for a zero vector; agree with `cosine()`, which says 0.0.
fn similarity_of(dist: Option<f64>) -> f32 {
    dist.map(|d| 1.0 - d as f32)
        .filter(|s| s.is_finite())
        .unwrap_or(0.0)
}

/// As Qdrant computes it: `1 / (rank + 2)` per half, summed. A lexical-only hit keeps no similarity.
fn fuse(dense: Vec<SearchHit>, lexical: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut by_id: HashMap<String, SearchHit> = HashMap::new();
    for (rank, mut h) in dense.into_iter().enumerate() {
        h.score = 1.0 / (rank as f32 + 2.0);
        by_id.insert(h.payload.artifact_id.clone(), h);
    }
    for (rank, mut h) in lexical.into_iter().enumerate() {
        let part = 1.0 / (rank as f32 + 2.0);
        match by_id.get_mut(&h.payload.artifact_id) {
            Some(seen) => seen.score += part,
            None => {
                h.score = part;
                h.similarity = None;
                by_id.insert(h.payload.artifact_id.clone(), h);
            }
        }
    }
    by_id.into_values().collect()
}

impl SqliteVectors {
    pub async fn connect(path: &std::path::Path, scoring: Scoring) -> Result<SqliteVectors> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        Self::open(opts, 2, scoring).await
    }

    /// One connection, because every connection to `:memory:` is its own base.
    pub async fn memory(scoring: Scoring) -> Result<SqliteVectors> {
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        Self::open(opts, 1, scoring).await
    }

    async fn open(
        opts: SqliteConnectOptions,
        conns: u32,
        scoring: Scoring,
    ) -> Result<SqliteVectors> {
        register();
        let pool = SqlitePoolOptions::new()
            .max_connections(conns)
            .connect_with(opts)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(SqliteVectors { pool, scoring })
    }

    /// The columns are the authority for the stamps laid over the payload.
    fn hydrate(row: &sqlx::sqlite::SqliteRow) -> Result<VectorPayload> {
        let mut p = decode(row.get::<&str, _>("payload"))?;
        p.last_seen_at = row.get("last_seen_at");
        p.hit_count = row.get("hit_count");
        p.last_verified_at = row.get("last_verified_at");
        Ok(p)
    }

    async fn write_point(
        tx: &mut sqlx::SqliteConnection,
        p: &VectorPayload,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        let status = status_word(p.status.unwrap_or(ArtifactStatus::Active));
        match embedding {
            Some(v) => {
                sqlx::query(
                    "INSERT INTO vec_points (artifact_id, corpus_id, category, status, created_at,
                        last_seen_at, hit_count, last_verified_at, payload, embedding)
                     VALUES (?,?,?,?,?,?,?,?,?,?)
                     ON CONFLICT(artifact_id) DO UPDATE SET corpus_id=excluded.corpus_id,
                        category=excluded.category, status=excluded.status,
                        created_at=excluded.created_at, last_seen_at=excluded.last_seen_at,
                        hit_count=excluded.hit_count,
                        last_verified_at=excluded.last_verified_at, payload=excluded.payload,
                        embedding=excluded.embedding",
                )
                .bind(&p.artifact_id)
                .bind(&p.corpus_id)
                .bind(&p.category)
                .bind(status)
                .bind(p.created_at)
                .bind(p.last_seen_at)
                .bind(p.hit_count)
                .bind(p.last_verified_at)
                .bind(encode(p))
                .bind(blob(v))
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "UPDATE vec_points SET corpus_id=?, category=?, status=?, created_at=?,
                        last_seen_at=?, hit_count=?, last_verified_at=?, payload=?
                     WHERE artifact_id=?",
                )
                .bind(&p.corpus_id)
                .bind(&p.category)
                .bind(status)
                .bind(p.created_at)
                .bind(p.last_seen_at)
                .bind(p.hit_count)
                .bind(p.last_verified_at)
                .bind(encode(p))
                .bind(&p.artifact_id)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query("DELETE FROM vec_tags WHERE artifact_id=?")
            .bind(&p.artifact_id)
            .execute(&mut *tx)
            .await?;
        for tag in &p.tags {
            sqlx::query("INSERT OR IGNORE INTO vec_tags (artifact_id, tag) VALUES (?,?)")
                .bind(&p.artifact_id)
                .bind(tag)
                .execute(&mut *tx)
                .await?;
        }
        Ok(())
    }

    async fn stored(tx: &mut sqlx::SqliteConnection, id: &str) -> Result<Option<VectorPayload>> {
        let row = sqlx::query(
            "SELECT payload, last_seen_at, hit_count, last_verified_at
             FROM vec_points WHERE artifact_id=?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        row.as_ref().map(Self::hydrate).transpose()
    }

    /// Text because the tag count varies; every value is bound, never spliced.
    fn where_clause(filter: &SearchFilter) -> (String, Vec<String>) {
        let mut sql = String::from("p.status <> 'trashed'");
        let mut binds = Vec::new();
        if !filter.include_superseded {
            sql.push_str(" AND p.status <> 'superseded'");
        }
        if !filter.include_deprecated {
            sql.push_str(" AND p.status <> 'deprecated'");
        }
        if let Some(c) = &filter.category {
            sql.push_str(" AND p.category = ?");
            binds.push(c.clone());
        }
        if let Some(c) = &filter.corpus_id {
            sql.push_str(" AND p.corpus_id = ?");
            binds.push(c.clone());
        }
        for t in &filter.tags {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM vec_tags t
                   WHERE t.artifact_id = p.artifact_id AND t.tag = ?)",
            );
            binds.push(t.clone());
        }
        (sql, binds)
    }

    /// `length(p.embedding) = ?` guards width: sqlite-vec fails the whole query on a mismatched row.
    async fn dense(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
        except: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let (clause, binds) = Self::where_clause(filter);
        let sql = format!(
            "SELECT p.payload, p.last_seen_at, p.hit_count, p.last_verified_at,
                    vec_distance_cosine(p.embedding, ?) AS dist
             FROM vec_points p
             WHERE length(p.embedding) = ? AND p.artifact_id IS NOT ? AND {clause}
             ORDER BY dist IS NULL, dist, p.artifact_id LIMIT ?"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
            .bind(blob(vector))
            .bind((vector.len() * 4) as i64)
            .bind(except);
        for b in &binds {
            q = q.bind(b);
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                let similarity = similarity_of(r.get("dist"));
                Ok(SearchHit {
                    payload: Self::hydrate(r)?,
                    score: similarity,
                    similarity: Some(similarity),
                })
            })
            .collect()
    }

    /// IDF as Qdrant's `idf` modifier: `ln((N - n + 0.5) / (n + 0.5) + 1)`, folded into the query weight.
    async fn lexical(
        &self,
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let total = self.count().await? as f32;
        let (clause, binds) = Self::where_clause(filter);
        let mut scores: HashMap<String, f32> = HashMap::new();
        for (term, qv) in sparse.indices.iter().zip(&sparse.values) {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_sparse WHERE term=?")
                .bind(*term as i64)
                .fetch_one(&self.pool)
                .await?;
            if n == 0 {
                continue;
            }
            let idf = ((total - n as f32 + 0.5) / (n as f32 + 0.5) + 1.0).ln();
            let sql = format!(
                "SELECT s.artifact_id, s.value FROM vec_sparse s
                 JOIN vec_points p ON p.artifact_id = s.artifact_id
                 WHERE s.term = ? AND {clause}"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone())).bind(*term as i64);
            for b in &binds {
                q = q.bind(b);
            }
            for r in q.fetch_all(&self.pool).await? {
                *scores.entry(r.get("artifact_id")).or_insert(0.0) +=
                    qv * idf * r.get::<f64, _>("value") as f32;
            }
        }
        let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
        let mut payloads = self.payloads_of(&ids).await?;
        Ok(ranked
            .into_iter()
            .filter_map(|(id, score)| {
                payloads.remove(&id).map(|payload| SearchHit {
                    payload,
                    score,
                    similarity: None,
                })
            })
            .collect())
    }

    /// `qdrant.rs::scoring_formula`'s arithmetic; never-verified points decay from now (neutral).
    fn apply_formula(&self, hits: &mut [SearchHit], recency: Recency) {
        let now = crate::store::now();
        let half_life = (recency.half_life_days.max(1) as f32) * 86_400.0;
        for h in hits {
            if recency.weight > 0.0 {
                let age = (now - h.payload.last_verified_at.unwrap_or(now)).max(0) as f32;
                h.score += recency.weight * 0.5f32.powf(age / half_life);
            }
            if self.scoring.pinned_boost > 0.0 && h.payload.tags.iter().any(|t| t == PINNED_TAG) {
                h.score += self.scoring.pinned_boost;
            }
        }
    }

    fn listed(rows: &[sqlx::sqlite::SqliteRow]) -> Result<Vec<SearchHit>> {
        rows.iter()
            .map(|r| {
                Ok(SearchHit {
                    payload: Self::hydrate(r)?,
                    score: 0.0,
                    similarity: None,
                })
            })
            .collect()
    }
}

#[async_trait]
impl VectorStore for SqliteVectors {
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        let have: Option<String> = sqlx::query_scalar("SELECT value FROM vec_meta WHERE key='dim'")
            .fetch_optional(&self.pool)
            .await?;
        match have.and_then(|d| d.parse::<usize>().ok()) {
            Some(d) if d == dim => Ok(()),
            Some(d) => Err(Error::Vector(format!(
                "this base was embedded at {d} dimensions and the embedder now gives {dim}"
            ))),
            None => {
                sqlx::query("INSERT OR REPLACE INTO vec_meta (key, value) VALUES ('dim', ?)")
                    .bind(dim.to_string())
                    .execute(&self.pool)
                    .await?;
                Ok(())
            }
        }
    }

    async fn upsert(&self, points: Vec<VectorPoint>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for mut p in points {
            // An unset stamp means "whatever is stored", never "clear it".
            if let Some(old) = Self::stored(&mut tx, &p.payload.artifact_id).await? {
                p.payload.last_seen_at = p.payload.last_seen_at.or(old.last_seen_at);
                p.payload.hit_count = p.payload.hit_count.or(old.hit_count);
                p.payload.status = p.payload.status.or(old.status);
                p.payload.last_verified_at = p.payload.last_verified_at.or(old.last_verified_at);
                p.payload.superseded_by = p.payload.superseded_by.or(old.superseded_by);
            }
            Self::write_point(&mut tx, &p.payload, Some(&p.vector)).await?;
            sqlx::query("DELETE FROM vec_sparse WHERE artifact_id=?")
                .bind(&p.payload.artifact_id)
                .execute(&mut *tx)
                .await?;
            for (term, value) in p.sparse.indices.iter().zip(&p.sparse.values) {
                sqlx::query(
                    "INSERT OR REPLACE INTO vec_sparse (term, artifact_id, value) VALUES (?,?,?)",
                )
                .bind(*term as i64)
                .bind(&p.payload.artifact_id)
                .bind(*value)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn set_payload(&self, payload: &VectorPayload) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(old) = Self::stored(&mut tx, &payload.artifact_id).await? else {
            return Ok(());
        };
        let mut p = payload.clone();
        p.last_seen_at = p.last_seen_at.or(old.last_seen_at);
        p.hit_count = p.hit_count.or(old.hit_count);
        p.status = p.status.or(old.status);
        p.last_verified_at = p.last_verified_at.or(old.last_verified_at);
        p.superseded_by = p.superseded_by.or(old.superseded_by);
        // Only the embed job knows these two; empty means unchanged.
        if p.origin_corpora.is_empty() {
            p.origin_corpora = old.origin_corpora;
        }
        p.provenance = p.provenance.or(old.provenance);
        Self::write_point(&mut tx, &p, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_lifecycle(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        superseded_by: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let Some(mut p) = Self::stored(&mut tx, artifact_id).await? else {
            return Ok(());
        };
        p.status = Some(status);
        p.superseded_by = superseded_by.map(str::to_string);
        Self::write_point(&mut tx, &p, None).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_last_verified_at(
        &self,
        artifact_id: &str,
        at: i64,
        reset_hits: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE vec_points SET last_verified_at=?,
                hit_count=CASE WHEN ? THEN 0 ELSE hit_count END
             WHERE artifact_id=?",
        )
        .bind(at)
        .bind(reset_hits)
        .bind(artifact_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn stale_candidates(
        &self,
        older_than: i64,
        max_hits: i64,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            "SELECT payload, last_seen_at, hit_count, last_verified_at FROM vec_points
             WHERE status='active' AND last_verified_at IS NOT NULL AND last_verified_at < ?
               AND COALESCE(hit_count, 0) <= ?
             ORDER BY last_verified_at, artifact_id LIMIT ?",
        )
        .bind(older_than)
        .bind(max_hits)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Self::listed(&rows)
    }

    async fn search(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        self.search_weighted(vector, sparse, limit, filter, self.scoring.recency)
            .await
    }

    async fn search_weighted(
        &self,
        vector: &[f32],
        sparse: &SparseVector,
        limit: usize,
        filter: &SearchFilter,
        recency: Recency,
    ) -> Result<Vec<SearchHit>> {
        let dense = self.dense(vector, limit, filter, None).await?;
        let mut hits = if sparse.is_empty() {
            // No indexable term: dense order, cosine as score, as in `qdrant.rs`.
            dense
        } else {
            let lexical = self.lexical(sparse, limit, filter).await?;
            fuse(dense, lexical)
        };
        self.apply_formula(&mut hits, recency);
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.payload.artifact_id.cmp(&b.payload.artifact_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn applies_scoring_formula(&self) -> bool {
        true
    }

    async fn touch(&self, targets: &[Touch], seen_at: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for t in targets {
            if t.counts_as_hit {
                sqlx::query(
                    "UPDATE vec_points SET last_seen_at=?,
                        hit_count=COALESCE(?, hit_count, 0) + 1
                     WHERE artifact_id=?",
                )
                .bind(seen_at)
                .bind(t.hit_count)
                .bind(&t.artifact_id)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("UPDATE vec_points SET last_seen_at=? WHERE artifact_id=?")
                    .bind(seen_at)
                    .bind(&t.artifact_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    async fn apply_lifecycle(&self, rows: &[LifecycleRow]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for r in rows {
            let Some(mut p) = Self::stored(&mut tx, &r.artifact_id).await? else {
                continue;
            };
            p.status = Some(r.status);
            p.superseded_by = r.superseded_by.clone();
            p.last_verified_at = Some(r.last_verified_at);
            Self::write_point(&mut tx, &p, None).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn lifecycle_of(
        &self,
        artifact_ids: &[String],
    ) -> Result<HashMap<String, StoredLifecycle>> {
        Ok(self
            .payloads_of(artifact_ids)
            .await?
            .into_iter()
            .map(|(id, p)| {
                (
                    id,
                    StoredLifecycle {
                        status: p.status.unwrap_or(ArtifactStatus::Active),
                        superseded_by: p.superseded_by,
                    },
                )
            })
            .collect())
    }

    async fn all_artifact_ids(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT artifact_id FROM vec_points")
            .fetch_all(&self.pool)
            .await?)
    }

    async fn payloads_of(&self, artifact_ids: &[String]) -> Result<HashMap<String, VectorPayload>> {
        let mut out = HashMap::new();
        let mut conn = self.pool.acquire().await?;
        for id in artifact_ids {
            if let Some(p) = Self::stored(&mut conn, id).await? {
                out.insert(id.clone(), p);
            }
        }
        Ok(out)
    }

    async fn resurface(
        &self,
        limit: usize,
        older_than: i64,
        unseen_since: i64,
    ) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            "SELECT payload, last_seen_at, hit_count, last_verified_at FROM vec_points
             WHERE status='active' AND created_at < ?
               AND (last_seen_at IS NULL OR last_seen_at < ?)
             ORDER BY random() LIMIT ?",
        )
        .bind(older_than)
        .bind(unseen_since)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Self::listed(&rows)
    }

    async fn facets(&self, limit: usize) -> Result<Facets> {
        let rows = sqlx::query(
            "SELECT category, COUNT(*) AS n FROM vec_points WHERE category IS NOT NULL
             GROUP BY category ORDER BY n DESC, category LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(Facets {
            categories: rows
                .iter()
                .map(|r| FacetCount {
                    value: r.get("category"),
                    count: r.get::<i64, _>("n") as u64,
                })
                .collect(),
        })
    }

    async fn neighbours(&self, artifact_id: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(v) = self.dense_of(artifact_id).await? else {
            return Ok(vec![]);
        };
        self.dense(&v, limit, &SearchFilter::default(), Some(artifact_id))
            .await
    }

    async fn set_context_vectors(&self, artifact_id: &str, vectors: Vec<Vec<f32>>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let there: Option<i64> = sqlx::query_scalar("SELECT 1 FROM vec_points WHERE artifact_id=?")
            .bind(artifact_id)
            .fetch_optional(&mut *tx)
            .await?;
        // No point is not an error, by the trait's word.
        if there.is_none() {
            return Ok(());
        }
        sqlx::query("DELETE FROM vec_ctx WHERE artifact_id=?")
            .bind(artifact_id)
            .execute(&mut *tx)
            .await?;
        for (n, v) in vectors.iter().enumerate() {
            sqlx::query("INSERT INTO vec_ctx (artifact_id, n, embedding) VALUES (?,?,?)")
                .bind(artifact_id)
                .bind(n as i64)
                .bind(blob(v))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn context_query(
        &self,
        vector: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let (clause, binds) = Self::where_clause(filter);
        // `max_sim`: the smallest distance in an artifact's set speaks for it.
        let sql = format!(
            "SELECT p.payload, p.last_seen_at, p.hit_count, p.last_verified_at,
                    MIN(vec_distance_cosine(c.embedding, ?)) AS dist
             FROM vec_ctx c JOIN vec_points p ON p.artifact_id = c.artifact_id
             WHERE length(c.embedding) = ? AND {clause}
             GROUP BY p.artifact_id ORDER BY dist, p.artifact_id LIMIT ?"
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
            .bind(blob(vector))
            .bind((vector.len() * 4) as i64);
        for b in &binds {
            q = q.bind(b);
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                Ok(SearchHit {
                    payload: Self::hydrate(r)?,
                    score: similarity_of(r.get("dist")),
                    similarity: None,
                })
            })
            .collect()
    }

    async fn delete_artifacts(&self, artifact_ids: &[String]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for id in artifact_ids {
            // The tags, postings and context set go with it: ON DELETE CASCADE.
            sqlx::query("DELETE FROM vec_points WHERE artifact_id=?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_by_corpus(&self, corpus_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM vec_points WHERE corpus_id=?")
            .bind(corpus_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn sample(&self, limit: usize) -> Result<Vec<(String, Vec<f32>)>> {
        let rows = sqlx::query(
            "SELECT artifact_id, embedding FROM vec_points ORDER BY artifact_id LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get("artifact_id"), unblob(r.get::<&[u8], _>("embedding"))))
            .collect())
    }

    async fn dense_of(&self, artifact_id: &str) -> Result<Option<Vec<f32>>> {
        let b: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT embedding FROM vec_points WHERE artifact_id=?")
                .bind(artifact_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(b.map(|b| unblob(&b)))
    }

    /// The vectors are rows in the tenant database, which the backup copies whole.
    async fn export_snapshot(&self, _into: &std::path::Path) -> Result<super::VectorExport> {
        Ok(super::VectorExport::Embedded)
    }

    async fn count(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vec_points")
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::conformance::{DIM, point, wide};
    use crate::vector::sparse::{encode_document, encode_query};

    mod conforms {
        crate::vector::conformance::suite!(async {
            Box::new(
                crate::vector::sqlite::SqliteVectors::memory(crate::vector::sqlite::Scoring::off())
                    .await
                    .unwrap(),
            )
        });
    }

    #[tokio::test]
    async fn what_was_written_is_there_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.db");
        {
            let s = SqliteVectors::connect(&path, Scoring::off()).await.unwrap();
            s.ensure_collection(DIM).await.unwrap();
            s.upsert(vec![point("a", "c", [1.0, 0.0, 0.0], &["t"], Some("note"))])
                .await
                .unwrap();
        }
        let s = SqliteVectors::connect(&path, Scoring::off()).await.unwrap();
        s.ensure_collection(DIM).await.unwrap();
        assert_eq!(s.count().await.unwrap(), 1);
        assert_eq!(s.dense_of("a").await.unwrap(), Some(vec![1.0, 0.0, 0.0]));
        let hits = s
            .search(&[1.0, 0.0, 0.0], &Default::default(), 5, &wide())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.tags, ["t"]);
    }

    #[tokio::test]
    async fn a_base_embedded_at_another_width_is_refused() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        let err = s.ensure_collection(4).await.unwrap_err();
        assert!(err.to_string().contains('3'), "{err}");
    }

    fn worded(id: &str, v: [f32; 3], text: &str) -> VectorPoint {
        let mut p = point(id, "c", v, &[], None);
        p.payload.text = text.into();
        p.sparse = encode_document(text);
        p
    }

    #[tokio::test]
    async fn an_exact_term_lifts_a_passage_the_dense_half_ranked_last() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            worded(
                "near",
                [1.0, 0.0, 0.0],
                "a note about nothing in particular",
            ),
            worded("mid", [0.9, 0.4, 0.0], "another note about nothing"),
            worded("term", [0.0, 1.0, 0.0], "run it with --dry-run first"),
        ])
        .await
        .unwrap();
        let q = [1.0, 0.0, 0.0];
        let dense_only = s.search(&q, &Default::default(), 3, &wide()).await.unwrap();
        assert_eq!(dense_only[2].payload.artifact_id, "term");
        let hybrid = s
            .search(&q, &encode_query("dry-run"), 3, &wide())
            .await
            .unwrap();
        // Last of three dense (1/4) plus first lexical (1/2) is 0.75, against 0.5 for the dense winner.
        assert_eq!(hybrid[0].payload.artifact_id, "term");
        assert!((hybrid[0].score - 0.75).abs() < 1e-5, "{}", hybrid[0].score);
        assert!(hybrid[0].similarity.is_some());
    }

    #[tokio::test]
    async fn a_lexical_only_hit_has_no_similarity() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            worded("a", [1.0, 0.0, 0.0], "plain"),
            worded("b", [0.9, 0.1, 0.0], "plain too"),
            worded("far", [0.0, 0.0, 1.0], "the zugzwang entry"),
        ])
        .await
        .unwrap();
        // Each half retrieves `limit`; at 2 the dense half never reaches `far`.
        let hits = s
            .search(&[1.0, 0.0, 0.0], &encode_query("zugzwang"), 2, &wide())
            .await
            .unwrap();
        let far = hits
            .iter()
            .find(|h| h.payload.artifact_id == "far")
            .expect("the lexical half found it");
        assert_eq!(far.similarity, None);
    }

    #[tokio::test]
    async fn a_rare_term_outweighs_a_common_one() {
        let s = SqliteVectors::memory(Scoring::off()).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        let mut pts: Vec<_> = (0..6)
            .map(|i| worded(&format!("c{i}"), [0.0, 0.0, 1.0], "common words"))
            .collect();
        pts.push(worded("rare", [0.0, 0.0, 1.0], "common quetzal"));
        s.upsert(pts).await.unwrap();
        // Fused, seven dense ties would order by id and say nothing about IDF.
        let hits = s
            .lexical(&encode_query("common quetzal"), 7, &wide())
            .await
            .unwrap();
        assert_eq!(hits[0].payload.artifact_id, "rare");
        assert_eq!(hits.len(), 7, "the common term still matches the rest");
    }

    #[tokio::test]
    async fn recency_breaks_a_tie_and_pinning_beats_recency() {
        let day = 86_400;
        let now = crate::store::now();
        let scoring = Scoring {
            recency: Recency {
                weight: 0.05,
                half_life_days: 30,
            },
            pinned_boost: 0.15,
        };
        let s = SqliteVectors::memory(scoring).await.unwrap();
        s.ensure_collection(3).await.unwrap();
        s.upsert(vec![
            point("old", "c", [1.0, 0.0, 0.0], &[], None),
            point("new", "c", [1.0, 0.0, 0.0], &[], None),
            point("pinned", "c", [1.0, 0.0, 0.0], &["pinned"], None),
        ])
        .await
        .unwrap();
        s.set_last_verified_at("old", now - 300 * day, false)
            .await
            .unwrap();
        s.set_last_verified_at("new", now, false).await.unwrap();
        s.set_last_verified_at("pinned", now - 300 * day, false)
            .await
            .unwrap();
        let hits = s
            .search(&[1.0, 0.0, 0.0], &Default::default(), 3, &wide())
            .await
            .unwrap();
        let order: Vec<_> = hits
            .iter()
            .map(|h| h.payload.artifact_id.as_str())
            .collect();
        assert_eq!(order, ["pinned", "new", "old"]);
        assert!(s.applies_scoring_formula());
        assert!(
            hits.iter()
                .all(|h| (h.similarity.unwrap() - 1.0).abs() < 1e-5)
        );
        let flat = s
            .search_weighted(
                &[1.0, 0.0, 0.0],
                &Default::default(),
                3,
                &wide(),
                Recency {
                    weight: 0.0,
                    half_life_days: 30,
                },
            )
            .await
            .unwrap();
        assert_eq!(flat[1].payload.artifact_id, "new");
        assert_eq!(flat[2].payload.artifact_id, "old");
    }
}
