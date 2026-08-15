use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedState {
    Pending,
    Embedded,
    Failed,
}

impl EmbedState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedState::Pending => "pending",
            EmbedState::Embedded => "embedded",
            EmbedState::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> EmbedState {
        match s {
            "embedded" => EmbedState::Embedded,
            "failed" => EmbedState::Failed,
            _ => EmbedState::Pending,
        }
    }
}

/// Where an artifact stands: still current, flagged stale with no named
/// replacement, or hidden in favour of a specific `superseded_by` artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Active,
    Deprecated,
    Superseded,
}

impl ArtifactStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactStatus::Active => "active",
            ArtifactStatus::Deprecated => "deprecated",
            ArtifactStatus::Superseded => "superseded",
        }
    }
    pub fn parse(s: &str) -> ArtifactStatus {
        match s {
            "deprecated" => ArtifactStatus::Deprecated,
            "superseded" => ArtifactStatus::Superseded,
            _ => ArtifactStatus::Active,
        }
    }
}

/// Where an artifact's text came from.
///
/// `Captured` text was written by synthesis over one window of one corpus, so
/// it has a corpus, a span, and lines to render beside it. `Merged` text was
/// written by the dedupe pass out of two or more captured artifacts; it has no
/// corpus and no span, and names its sources through `artifact_sources`
/// instead.
///
/// Nothing may treat the two alike. `verify` cannot check a merged artifact
/// against a segment that does not exist, and the detail pane cannot render
/// corpus lines for a span it does not have — so both branch on this rather
/// than on `corpus_id.is_none()`. A null says a field is absent; this says what
/// the row *is*, which is what a reader needs in order to know why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Captured,
    Merged,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Captured => "captured",
            Provenance::Merged => "merged",
        }
    }
    pub fn parse(s: &str) -> Provenance {
        match s {
            "merged" => Provenance::Merged,
            _ => Provenance::Captured,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CorpusSpan {
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Chunk {
    pub id: String,
    /// `None` for a merged artifact, which belongs to no single corpus. Branch
    /// on `provenance`, not on this: see `Provenance`.
    pub corpus_id: Option<String>,
    pub provenance: Provenance,
    /// How many artifacts a merge was written from, so a source deleted since
    /// can be noticed. Zero for a captured artifact.
    pub source_count: i64,
    pub ordinal: i64,
    pub text: String,
    pub corpus_span: Option<CorpusSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub embed_state: EmbedState,
    pub embed_model: Option<String>,
    pub created_at: i64,
    /// Bumped by every edit that invalidates the stored vector. Internal
    /// bookkeeping between the editor and the embed job, so it is not part of
    /// what the API hands out.
    #[serde(skip)]
    pub embed_rev: i64,
    /// Which segmentation window produced this chunk. `None` for chunks
    /// written before per-window segmentation existed.
    pub segment_idx: Option<i64>,
    /// Verification failures. Empty means every check passed.
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    /// The artifact this one lost a near-identical pair to. Set by the
    /// consolidation sweep; the artifact stays stored and readable, and is only
    /// kept out of ranking.
    pub superseded_by: Option<String>,
    /// Conditions under which this artifact does not apply, as its source
    /// stated them. Deliberately not part of what gets embedded: changing what
    /// every vector is built from is a decision for the evaluation harness.
    pub caveats: Vec<String>,
    /// Active, deprecated, or superseded. Kept in sync with `superseded_by`
    /// by `set_superseded_by`; set directly by `set_artifact_status` for the
    /// deprecate/reactivate actions, which have no artifact on the other end.
    pub status: ArtifactStatus,
    /// When this artifact was last confirmed accurate. Defaults to
    /// `created_at` at insert; this, not `created_at`, is what search ranking
    /// decays against.
    pub last_verified_at: Option<i64>,
}

impl Chunk {
    /// Whether search may return this artifact: active and not hidden behind
    /// a winner. This is the predicate every consolidation decision gates on —
    /// what may win a cluster, be shown to the model, or be superseded — so it
    /// has exactly one spelling. A third lifecycle state changes this method,
    /// not a dozen call sites.
    pub fn in_results(&self) -> bool {
        self.status == ArtifactStatus::Active && self.superseded_by.is_none()
    }
}

/// A merged artifact being created.
///
/// Deliberately not `NewArtifact`. There is no corpus, no span, no segment and
/// no position within a document, and a struct carrying those as `None` invites
/// a caller to fill one in — which is exactly the claim a merged artifact must
/// not make.
#[derive(Debug, Clone)]
pub struct NewMerged {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NewArtifact {
    pub ordinal: i64,
    pub text: String,
    pub corpus_span: Option<CorpusSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub segment_idx: Option<i64>,
    pub caveats: Vec<String>,
}

pub(crate) fn row_to_artifact(r: &sqlx::sqlite::SqliteRow) -> Chunk {
    let tags_json: String = r.get("tags");
    let span_json: Option<String> = r.get("corpus_span");
    let flags_json: Option<String> = r.get("flags");
    Chunk {
        id: r.get("id"),
        corpus_id: r.get("corpus_id"),
        provenance: Provenance::parse(r.get::<String, _>("provenance").as_str()),
        source_count: r.get("source_count"),
        ordinal: r.get("ordinal"),
        text: r.get("text"),
        corpus_span: span_json.and_then(|s| serde_json::from_str(&s).ok()),
        title: r.get("title"),
        category: r.get("category"),
        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        embed_state: EmbedState::parse(r.get::<String, _>("embed_state").as_str()),
        embed_model: r.get("embed_model"),
        created_at: r.get("created_at"),
        embed_rev: r.get("embed_rev"),
        segment_idx: r.get("segment_idx"),
        flags: flags_json
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        flag_detail: r.get("flag_detail"),
        superseded_by: r.get("superseded_by"),
        caveats: r
            .get::<Option<String>, _>("caveats")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        status: ArtifactStatus::parse(r.get::<String, _>("status").as_str()),
        last_verified_at: r.get("last_verified_at"),
    }
}

impl Store {
    /// Write a merged artifact and its lineage in one transaction.
    ///
    /// One transaction, not two writes. A merged artifact with no lineage rows
    /// is one whose detail pane can render nothing and whose sources nobody can
    /// recover — and the re-merge rule reads exactly those rows to avoid ever
    /// rewriting from text a model produced. Splitting the writes makes that
    /// state reachable by a crash, and nothing afterwards could tell it from an
    /// artifact whose sources were all deleted.
    ///
    /// `sources` may name merged artifacts. They are flattened to their own
    /// captured roots here, so `artifact_sources.root_id` only ever names a
    /// `captured` artifact — the invariant the whole anti-drift rule rests on.
    pub async fn insert_merged_artifact(
        &self,
        new: &NewMerged,
        sources: &[String],
    ) -> Result<Chunk> {
        // Resolved before the transaction opens: this is a read, and holding a
        // write transaction across it buys nothing.
        let resolved = self.roots_of(sources).await?;
        // The roots actually about to be written, deduped the way the table
        // dedupes them: `artifact_sources` is keyed on (child_id, root_id), so
        // two sources sharing a root produce one row.
        //
        // Counting `sources` instead counted the inputs, which for a merge of a
        // merge is fewer than the roots — M2 = merge(M1(a,b), c) recorded two
        // against three rows. `merged_missing_a_source` asks whether the count
        // exceeds the surviving rows, so deleting a root left 2 > 2, false, and
        // the orphan flag never fired for a merge of a merge at all. That is
        // precisely the case the counter was added for.
        let root_ids: std::collections::BTreeSet<&String> = resolved.values().flatten().collect();

        let mut tx = self.pool.begin().await?;
        let created_at = now();
        let c = Chunk {
            id: new_id(),
            corpus_id: None,
            provenance: Provenance::Merged,
            source_count: root_ids.len() as i64,
            ordinal: 0,
            text: new.text.clone(),
            corpus_span: None,
            title: new.title.clone(),
            category: new.category.clone(),
            tags: new.tags.clone(),
            embed_state: EmbedState::Pending,
            embed_model: None,
            created_at,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: new.caveats.clone(),
            status: ArtifactStatus::Active,
            last_verified_at: Some(created_at),
        };
        sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, provenance, source_count, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at)
             VALUES (?, NULL, 'merged', ?, 0, ?, NULL, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?)",
        )
        .bind(&c.id)
        .bind(c.source_count)
        .bind(&c.text)
        .bind(&c.title)
        .bind(&c.category)
        .bind(serde_json::to_string(&c.tags).unwrap())
        .bind(c.embed_state.as_str())
        .bind(c.created_at)
        .bind(serde_json::to_string(&c.caveats).unwrap_or_else(|_| "[]".into()))
        .bind(c.status.as_str())
        .bind(c.last_verified_at)
        .execute(&mut *tx)
        .await?;

        for (via, roots) in &resolved {
            for root in roots {
                // OR IGNORE because two sources can share a root: merging M(a,b)
                // with a itself is a component the sweep can legitimately build,
                // and it names `a` twice.
                sqlx::query(
                    "INSERT OR IGNORE INTO artifact_sources (child_id, root_id, via_id, created_at)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(&c.id)
                .bind(root)
                .bind(via)
                .bind(created_at)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(c)
    }

    pub async fn insert_artifacts(
        &self,
        corpus_id: &str,
        chunks: &[NewArtifact],
    ) -> Result<Vec<Chunk>> {
        let mut tx = self.pool.begin().await?;
        let mut out = Vec::with_capacity(chunks.len());
        for nc in chunks {
            let created_at = now();
            let c = Chunk {
                id: new_id(),
                corpus_id: Some(corpus_id.to_string()),
                provenance: Provenance::Captured,
                source_count: 0,
                ordinal: nc.ordinal,
                text: nc.text.clone(),
                corpus_span: nc.corpus_span.clone(),
                title: nc.title.clone(),
                category: nc.category.clone(),
                tags: nc.tags.clone(),
                embed_state: EmbedState::Pending,
                embed_model: None,
                created_at,
                embed_rev: 0,
                segment_idx: nc.segment_idx,
                flags: vec![],
                flag_detail: None,
                superseded_by: None,
                caveats: nc.caveats.clone(),
                status: ArtifactStatus::Active,
                last_verified_at: Some(created_at),
            };
            sqlx::query(
                "INSERT INTO artifacts (id, corpus_id, provenance, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at)
                 VALUES (?, ?, 'captured', ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?)",
            )
            .bind(&c.id)
            .bind(&c.corpus_id)
            .bind(c.ordinal)
            .bind(&c.text)
            .bind(c.corpus_span.as_ref().map(|s| serde_json::to_string(s).unwrap()))
            .bind(&c.title)
            .bind(&c.category)
            .bind(serde_json::to_string(&c.tags).unwrap())
            .bind(c.embed_state.as_str())
            .bind(c.created_at)
            .bind(c.segment_idx)
            .bind(serde_json::to_string(&c.caveats).unwrap_or_else(|_| "[]".into()))
            .bind(c.status.as_str())
            .bind(c.last_verified_at)
            .execute(&mut *tx)
            .await?;
            out.push(c);
        }
        tx.commit().await?;
        Ok(out)
    }

    pub async fn get_artifact(&self, id: &str) -> Result<Chunk> {
        let row = sqlx::query("SELECT * FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_artifact(&row))
    }

    /// Every artifact an ordinary search could return.
    ///
    /// Superseded and deprecated stay out, so a benchmark built from this sees
    /// the same base the search page does. Each artifact carries its
    /// `corpus_id`, which is what the per-corpus cap groups by: a title hint
    /// reads better but is not unique, and two captures of the same document
    /// merged into one source made the cap apply across both.
    pub async fn all_active_artifacts(&self) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts
             WHERE status = 'active' AND superseded_by IS NULL
             ORDER BY corpus_id, ordinal",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    pub async fn artifacts_for_corpus(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query("SELECT * FROM artifacts WHERE corpus_id = ? ORDER BY ordinal")
            .bind(corpus_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// How many chunks a source produced, without loading any of them.
    ///
    /// The queue fragment wants the number and nothing else, and it asks for
    /// ten sources every three seconds while anything is in flight — reading
    /// every artifact's full text to call `.len()` on the result was the bulk
    /// of that poll.
    pub async fn count_artifacts_for_corpus(&self, corpus_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE corpus_id = ?")
                .bind(corpus_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Chunks of a source still waiting for a vector. The embed job batches
    /// these into one inference call, so it needs them as rows, not a count.
    pub async fn pending_artifacts_for_corpus(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE corpus_id = ? AND embed_state = 'pending' ORDER BY ordinal",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Is every chunk still waiting to be embedded already armed as its own
    /// unit? True means the source has been taken off the batch path and onto
    /// the per-chunk one, and re-arming its batch would only spend another call
    /// discovering the same refusal.
    pub async fn pending_artifacts_are_isolated(&self, corpus_id: &str) -> Result<bool> {
        let unarmed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM artifacts a
              WHERE a.corpus_id = ? AND a.embed_state = 'pending'
                AND NOT EXISTS (
                  SELECT 1 FROM jobs j
                   WHERE j.stage = 'embed' AND j.target_id = a.id AND j.state != 'done'
                )",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(unarmed == 0)
    }

    /// Put every chunk of a source back in the embed queue's path. Re-embedding
    /// only happens for rows that say they still need it, so asking for it has
    /// to say so first.
    ///
    /// The revision bump is what makes this safe to run while a worker is
    /// mid-batch on the same source: that worker's `mark_embedded` no longer
    /// matches, so it cannot clear the pending state this just set.
    pub async fn reset_embed_state(&self, corpus_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE artifacts
             SET embed_state = 'pending', embed_model = NULL, embed_rev = embed_rev + 1
             WHERE corpus_id = ?",
        )
        .bind(corpus_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set or clear the category. Deliberately does not touch `embed_state`:
    /// the embedding model is never shown a category, so the stored vector is
    /// still correct.
    pub async fn update_artifact_category(&self, id: &str, category: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET category = ? WHERE id = ?")
                .bind(category)
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Replace the tag list. An empty list is a clear, not a no-op.
    pub async fn update_artifact_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET tags = ? WHERE id = ?")
                .bind(serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()))
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// The title is part of the text handed to the embedder, so setting or
    /// clearing it invalidates the vector the same way changing the body does.
    pub async fn update_artifact_title(&self, id: &str, title: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query(
                "UPDATE artifacts
                 SET title = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1
                 WHERE id = ?",
            )
            .bind(title)
            .bind(id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn update_artifact_text(&self, id: &str, text: &str) -> Result<()> {
        self.expect_updated(
            sqlx::query(
                "UPDATE artifacts
                 SET text = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1
                 WHERE id = ?",
            )
            .bind(text)
            .bind(id)
            .execute(&self.pool)
            .await?,
        )
    }

    fn expect_updated(&self, res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Report a chunk indexed, but only if it has not been edited since the
    /// embed job read it.
    ///
    /// Returns whether the mark landed. `false` means a newer revision exists
    /// and the vector just written describes text that is already stale; the
    /// chunk stays pending, so it will be embedded again from the current row.
    ///
    /// That relies on an invariant worth keeping: whoever bumps the revision
    /// also queues the work. `update_artifact_text`, `update_artifact_title` and
    /// `reset_embed_state` are only ever called alongside an `enqueue`, so a
    /// chunk left pending here always has a job coming for it.
    pub async fn mark_embedded(&self, id: &str, model: &str, rev: i64) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE artifacts SET embed_state = 'embedded', embed_model = ?
             WHERE id = ? AND embed_rev = ?",
        )
        .bind(model)
        .bind(id)
        .bind(rev)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_embed_failed(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE artifacts SET embed_state = 'failed' WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// The chunks a window's next write replaces.
    ///
    /// Chunks with no window at all are included, because a source segmented
    /// before windows existed has nothing else to key on: leaving them out
    /// would append the new segmentation beside the old one instead of
    /// replacing it. They are swept by whichever window writes first, and there
    /// are none left by the second.
    pub async fn artifact_ids_for_segment(
        &self,
        corpus_id: &str,
        segment_idx: i64,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM artifacts WHERE corpus_id = ?
               AND (segment_idx = ? OR segment_idx IS NULL)
             ORDER BY ordinal",
        )
        .bind(corpus_id)
        .bind(segment_idx)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Open a gap of `by` ordinals after `ordinal`, so chunks inserted into it
    /// keep reading order without renumbering the whole source.
    pub async fn make_room_after(&self, corpus_id: &str, ordinal: i64, by: i64) -> Result<()> {
        sqlx::query(
            "UPDATE artifacts SET ordinal = ordinal + ? WHERE corpus_id = ? AND ordinal > ?",
        )
        .bind(by)
        .bind(corpus_id)
        .bind(ordinal)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Give a source one continuous ordinal sequence again.
    ///
    /// Chunks are inserted per window and numbered within it, so until this
    /// runs a source has three chunks numbered 0. Ordering by window and then
    /// by the within-window number reproduces reading order.
    pub async fn renumber_artifacts(&self, corpus_id: &str) -> Result<()> {
        let rows = sqlx::query(
            "SELECT id FROM artifacts WHERE corpus_id = ?
             ORDER BY COALESCE(segment_idx, 0), ordinal, rowid",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        for (n, r) in rows.iter().enumerate() {
            sqlx::query("UPDATE artifacts SET ordinal = ? WHERE id = ?")
                .bind(n as i64)
                .bind(r.get::<String, _>("id"))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record that this artifact lost a near-identical pair. `None` undoes it.
    /// `status` moves in lockstep: superseded when a winner is set, active
    /// when it's cleared — the one place that keeps the two columns
    /// consistent, so callers of `unsupersede`/`heal_dangling_supersessions`
    /// need no changes of their own.
    pub async fn set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()> {
        let status = if by.is_some() {
            ArtifactStatus::Superseded
        } else {
            ArtifactStatus::Active
        };
        // The marker rides the same statement as the change it describes. Two
        // statements could be interrupted between, which is the exact failure
        // this is here to catch — a lifecycle change that never reached the
        // payload and that nothing afterwards knows to look for.
        let res = sqlx::query(
            "UPDATE artifacts SET superseded_by = ?, status = ?, lifecycle_dirty = 1 WHERE id = ?",
        )
        .bind(by)
        .bind(status.as_str())
        .bind(artifact_id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Set an artifact's lifecycle status directly — used for deprecate and
    /// reactivate, which (unlike supersede) have no winning artifact on the
    /// other end. Does not touch `superseded_by`; callers that mean to clear
    /// a supersession should use `set_superseded_by(id, None)` instead.
    pub async fn set_artifact_status(&self, id: &str, status: ArtifactStatus) -> Result<()> {
        // Marked dirty in the same statement, like `set_superseded_by`. See
        // `dirty_lifecycle_artifacts`.
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET status = ?, lifecycle_dirty = 1 WHERE id = ?")
                .bind(status.as_str())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Artifacts whose lifecycle row has changed since the payload was last
    /// written. The drift repair's whole work list.
    ///
    /// This replaced a pair of capped scans over every non-active artifact.
    /// Merging makes hidden artifacts grow monotonically — every merge hides at
    /// least two, permanently — so those scans were on course to be truncated
    /// forever, repairing a shifting window of an ever-growing set while
    /// reporting success either way. What needs repairing is the writes that
    /// did not finish, and that set is almost always empty.
    pub async fn dirty_lifecycle_artifacts(&self, limit: usize) -> Result<Vec<Chunk>> {
        let rows =
            sqlx::query("SELECT * FROM artifacts WHERE lifecycle_dirty = 1 ORDER BY id LIMIT ?")
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Mark an artifact's lifecycle as in flight before either store is
    /// written.
    ///
    /// For `supersede` and `deprecate` this is redundant — the row write sets
    /// the marker itself, and it comes first. The reveal direction is why this
    /// exists: `reactivate` and `unsupersede` write the *payload* first, on
    /// purpose, so that a half-finished reveal leaves the artifact visible
    /// rather than hidden behind a payload no page explains. Without marking
    /// up front, a crash between the two stores leaves drift that no row write
    /// ever announced, and the marker would miss the one direction whose
    /// intermediate state it was most needed for.
    pub async fn mark_lifecycle_dirty(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE artifacts SET lifecycle_dirty = 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Clear the marker once the payload write has been acknowledged.
    ///
    /// Never before it. Clearing first turns a failed payload write into
    /// permanent drift with nothing left that knows to look for it, which is
    /// precisely the state this mechanism exists to make impossible.
    pub async fn clear_lifecycle_dirty(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE artifacts SET lifecycle_dirty = 0 WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Stamp an artifact as confirmed accurate now — what search ranking's
    /// recency decay reads.
    pub async fn set_last_verified_at(&self, id: &str, at: i64) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET last_verified_at = ? WHERE id = ?")
                .bind(at)
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Every artifact id, for the one-shot Qdrant lifecycle backfill.
    pub async fn list_all_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Ids of artifacts whose row claims a vector was written for them.
    ///
    /// The heal uses this rather than `list_all_artifact_ids` for the
    /// SQLite-has-it/vectors-do-not direction, because an artifact still waiting
    /// on its embed job has no point *correctly* — that is the normal state of
    /// everything just ingested, not drift. Nor is a `failed` row: the embedder
    /// refused that text, and re-queueing it every sweep is a retry loop with no
    /// end. Only a row that says `embedded` while the vector store holds nothing
    /// is a write that went missing.
    pub async fn list_embedded_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts WHERE embed_state = 'embedded'")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Live, embedded artifacts whose `Relate` unit was never armed — the
    /// backstop for an arming that failed after the embed committed. A row
    /// survives its completion, so "no job at all" is exactly "never asked".
    pub async fn list_unrelated_artifact_ids(&self, limit: usize) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT a.id FROM artifacts a
              WHERE a.status = 'active' AND a.superseded_by IS NULL
                AND a.embed_state = 'embedded'
                AND NOT EXISTS (SELECT 1 FROM jobs j
                                 WHERE j.stage = 'relate' AND j.target_id = a.id)
              ORDER BY a.created_at
              LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Artifacts hidden in favour of a keeper that no longer exists.
    ///
    /// `superseded_by` is a plain column with no foreign key, so deleting a
    /// corpus — or reprocessing one, which deletes and re-creates every
    /// artifact under new ids — can leave the losing side of a pair pointing at
    /// nothing. That artifact is hidden from search forever, in favour of a
    /// copy that is gone: the surviving text becomes invisible, which is the
    /// exact loss consolidation exists to avoid.
    ///
    /// A read, not a write: the caller clears the vector payload before the
    /// row, so that a failure between the two leaves the artifact listed on Ops
    /// rather than hidden with nothing pointing at it. See
    /// `Core::heal_dangling_supersessions`.
    pub async fn dangling_superseded(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM artifacts
              WHERE superseded_by IS NOT NULL
                AND superseded_by NOT IN (SELECT id FROM artifacts)",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Artifacts hidden in favour of this one.
    ///
    /// What a supersession chain is made of. When the winner is itself
    /// superseded, everything pointing at it has to be re-pointed or the reader
    /// who opens one of these is sent to an artifact that is not in results
    /// either — a dead end no page can follow.
    pub async fn artifacts_superseded_by(&self, winner_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts WHERE superseded_by = ?")
            .bind(winner_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Artifacts currently hidden by consolidation, newest first.
    pub async fn superseded_artifacts(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE superseded_by IS NOT NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Artifacts currently carrying one lifecycle status, newest first. Used
    /// for the Ops "deprecated" list — superseded artifacts have their own
    /// query (`superseded_artifacts`) since that one also needs the winner.
    pub async fn artifacts_by_status(
        &self,
        status: ArtifactStatus,
        limit: i64,
    ) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE status = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(status.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Record what verification found. An empty list clears the flags, so a
    /// re-checked chunk does not keep a warning it no longer earns.
    pub async fn set_artifact_flags(
        &self,
        id: &str,
        flags: &[String],
        detail: Option<&str>,
    ) -> Result<()> {
        let json = if flags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(flags).unwrap_or_else(|_| "[]".into()))
        };
        sqlx::query("UPDATE artifacts SET flags = ?, flag_detail = ? WHERE id = ?")
            .bind(json)
            .bind(detail)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear_artifact_flags(&self, id: &str) -> Result<()> {
        self.set_artifact_flags(id, &[], None).await
    }

    /// `Chunk::in_results`, answered from two columns instead of the full row.
    ///
    /// For callers that triage many artifacts and read the text of few —
    /// `arm_dedupe` walks up to 200 pairs per tick and skips most of them, and
    /// paying two full-row fetches (text included) per skipped pair was the
    /// bulk of the tick's work. `None` means the row is gone, which is its own
    /// answer rather than an error: a pair cascades away mid-loop and the
    /// caller moves on.
    pub async fn artifact_in_results(&self, id: &str) -> Result<Option<bool>> {
        let row = sqlx::query(
            "SELECT status = 'active' AND superseded_by IS NULL AS live
               FROM artifacts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<bool, _>("live")))
    }

    /// Record that an operator reviewed a merge's lost sources and accepted it
    /// as a merge of what remains. `source_count` comes down to the surviving
    /// lineage count, so the orphan scan's comparison goes quiet — without
    /// this, clearing the flag lasts exactly one sweep, because the row still
    /// answers "lost a source" and is flagged all over again.
    pub async fn accept_source_loss(&self, id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE artifacts
                SET source_count = (SELECT COUNT(*) FROM artifact_sources WHERE child_id = ?)
              WHERE id = ? AND provenance = 'merged'",
        )
        .bind(id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_by_embed_state(&self, corpus_id: &str, state: &str) -> Result<i64> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM artifacts WHERE corpus_id = ? AND embed_state = ?",
        )
        .bind(corpus_id)
        .bind(state)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("n"))
    }

    pub async fn pending_embed_count(&self, corpus_id: &str) -> Result<i64> {
        self.count_by_embed_state(corpus_id, "pending").await
    }

    pub async fn failed_embed_count(&self, corpus_id: &str) -> Result<i64> {
        self.count_by_embed_state(corpus_id, "failed").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn nc(ord: i64, text: &str) -> NewArtifact {
        NewArtifact {
            ordinal: ord,
            text: text.to_string(),
            corpus_span: Some(CorpusSpan {
                start_line: 1,
                end_line: 4,
            }),
            caveats: vec![],
            title: Some(format!("title {ord}")),
            category: Some("procedure".into()),
            tags: vec!["forensics".into(), "windows".into()],
            segment_idx: None,
        }
    }

    #[tokio::test]
    async fn a_captured_artifact_is_captured_and_names_its_corpus() {
        // `provenance` is the discriminator every consumer branches on, never
        // `corpus_id IS NULL`. A null is an absence; a kind is an assertion,
        // and the failure modes merging can produce want to hang off an
        // assertion — `verify` cannot check a merged artifact against a segment
        // that does not exist, and the detail pane cannot render lines for a
        // span it does not have.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s.insert_artifacts(&src.id, &[nc(0, "one")]).await.unwrap();

        assert_eq!(made[0].provenance, Provenance::Captured);
        assert_eq!(made[0].corpus_id.as_deref(), Some(src.id.as_str()));

        let read = s.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(read.provenance, Provenance::Captured);
        assert_eq!(read.corpus_id.as_deref(), Some(src.id.as_str()));
        assert_eq!(
            read.source_count, 0,
            "a captured artifact was merged from something"
        );
    }

    #[tokio::test]
    async fn chunks_are_replaced_per_window_not_per_source() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut a = nc(0, "window zero");
        a.segment_idx = Some(0);
        let mut b = nc(0, "window one");
        b.segment_idx = Some(1);
        s.insert_artifacts(&src.id, &[a, b]).await.unwrap();

        let ids = s.artifact_ids_for_segment(&src.id, 1).await.unwrap();
        assert_eq!(ids.len(), 1);
        for id in &ids {
            s.delete_artifact(id).await.unwrap();
        }

        let left = s.artifacts_for_corpus(&src.id).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].text, "window zero");
    }

    #[tokio::test]
    async fn renumbering_orders_by_window_then_position() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut second = nc(1, "second of window one");
        second.segment_idx = Some(1);
        let mut first = nc(0, "first of window one");
        first.segment_idx = Some(1);
        let mut zero = nc(0, "only of window zero");
        zero.segment_idx = Some(0);
        s.insert_artifacts(&src.id, &[second, first, zero])
            .await
            .unwrap();

        s.renumber_artifacts(&src.id).await.unwrap();
        let got = s.artifacts_for_corpus(&src.id).await.unwrap();
        assert_eq!(got[0].text, "only of window zero");
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[1].text, "first of window one");
        assert_eq!(got[1].ordinal, 1);
        assert_eq!(got[2].ordinal, 2);
    }

    #[tokio::test]
    async fn flags_round_trip() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "clean"), nc(1, "suspect")])
            .await
            .unwrap();

        s.set_artifact_flags(
            &made[1].id,
            &["literals_unverified".to_string()],
            Some("missing literal: --dry-run"),
        )
        .await
        .unwrap();

        let flagged = s.get_artifact(&made[1].id).await.unwrap();
        assert_eq!(flagged.flags, vec!["literals_unverified".to_string()]);
        assert_eq!(
            flagged.flag_detail.as_deref(),
            Some("missing literal: --dry-run")
        );

        s.clear_artifact_flags(&made[1].id).await.unwrap();
        assert!(s.get_artifact(&made[1].id).await.unwrap().flags.is_empty());
    }

    #[tokio::test]
    async fn coverage_is_stored_on_the_source() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.set_corpus_coverage(&src.id, 0.42).await.unwrap();
        let got = s.get_corpus(&src.id).await.unwrap();
        assert!((got.coverage.unwrap() - 0.42).abs() < 1e-6);
    }

    #[tokio::test]
    async fn insert_and_read_back_chunks() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "## A\nfirst"), nc(1, "## B\nsecond")])
            .await
            .unwrap();
        assert_eq!(made.len(), 2);

        let got = s.artifacts_for_corpus(&src.id).await.unwrap();
        assert_eq!(got[0].ordinal, 0);
        assert_eq!(got[1].text, "## B\nsecond");
        assert_eq!(
            got[0].tags,
            vec!["forensics".to_string(), "windows".to_string()]
        );
        assert_eq!(got[0].corpus_span.as_ref().unwrap().end_line, 4);
        assert_eq!(got[0].embed_state, EmbedState::Pending);
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_chunks() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.insert_artifacts(&src.id, &[nc(0, "x")]).await.unwrap();
        s.delete_corpus(&src.id).await.unwrap();
        assert!(s.artifacts_for_corpus(&src.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn editing_text_resets_embed_state() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let c = s
            .insert_artifacts(&src.id, &[nc(0, "x")])
            .await
            .unwrap()
            .remove(0);
        assert!(s.mark_embedded(&c.id, "bge-m3", c.embed_rev).await.unwrap());
        assert_eq!(
            s.get_artifact(&c.id).await.unwrap().embed_state,
            EmbedState::Embedded
        );

        s.update_artifact_text(&c.id, "## x\nedited").await.unwrap();
        let after = s.get_artifact(&c.id).await.unwrap();
        assert_eq!(after.text, "## x\nedited");
        assert_eq!(
            after.embed_state,
            EmbedState::Pending,
            "edited text must not keep a stale vector"
        );
    }

    #[tokio::test]
    async fn counts_track_embed_progress() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        assert_eq!(s.pending_embed_count(&src.id).await.unwrap(), 2);

        s.mark_embedded(&made[0].id, "m", made[0].embed_rev)
            .await
            .unwrap();
        s.mark_embed_failed(&made[1].id).await.unwrap();
        assert_eq!(s.pending_embed_count(&src.id).await.unwrap(), 0);
        assert_eq!(s.failed_embed_count(&src.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn listing_dangling_supersessions_does_not_clear_them() {
        // The healing order depends on this being a read. If listing also
        // cleared the rows, a vector write that then failed would leave the
        // artifact hidden with `superseded_by` already NULL: off the Ops list,
        // past the sweep's self-heal branch, and unreachable by any button.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "loser".into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        s.set_superseded_by(&made[0].id, Some("an-artifact-that-is-gone"))
            .await
            .unwrap();

        assert_eq!(
            s.dangling_superseded().await.unwrap(),
            vec![made[0].id.clone()]
        );
        assert_eq!(
            s.dangling_superseded().await.unwrap(),
            vec![made[0].id.clone()],
            "the second call came back empty, so the first one wrote"
        );
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .superseded_by
                .is_some()
        );
    }

    #[tokio::test]
    async fn in_results_means_active_and_not_superseded() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "one"), nc(1, "two")])
            .await
            .unwrap();

        assert!(s.get_artifact(&made[0].id).await.unwrap().in_results());
        s.set_superseded_by(&made[0].id, Some(&made[1].id))
            .await
            .unwrap();
        assert!(!s.get_artifact(&made[0].id).await.unwrap().in_results());
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert!(!s.get_artifact(&made[1].id).await.unwrap().in_results());
    }

    #[tokio::test]
    async fn liveness_is_answerable_without_fetching_the_row() {
        // What `arm_dedupe` reads 200 times per tick, most of them for pairs
        // it goes on to skip — two columns, never the text.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "one"), nc(1, "two")])
            .await
            .unwrap();

        assert_eq!(
            s.artifact_in_results(&made[0].id).await.unwrap(),
            Some(true)
        );
        s.set_superseded_by(&made[0].id, Some(&made[1].id))
            .await
            .unwrap();
        assert_eq!(
            s.artifact_in_results(&made[0].id).await.unwrap(),
            Some(false)
        );
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert_eq!(
            s.artifact_in_results(&made[1].id).await.unwrap(),
            Some(false)
        );
        // A missing row is its own answer, not an error: the caller treats a
        // cascaded-away pair differently from a store that is unwell.
        assert_eq!(s.artifact_in_results("no-such-id").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_fts_index_is_gone() {
        // The lexical half of search lives in Qdrant now. This asserts the
        // SQLite index and its three write triggers were actually dropped,
        // rather than left behind to be paid for on every artifact write.
        let s = Store::memory().await.unwrap();
        let leftovers: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master
             WHERE name = 'artifacts_fts' OR name LIKE 'artifacts_a%'",
        )
        .fetch_all(&s.pool)
        .await
        .unwrap();
        assert!(leftovers.is_empty(), "fts leftovers: {leftovers:?}");
    }
}
