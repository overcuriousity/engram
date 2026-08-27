use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;
use std::collections::HashMap;

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
///
/// `Passage` text is a verbatim slice of a segment, sized to the embedder —
/// the retrieval unit at `synthesis = "off"` and `"earned"`; it has a corpus
/// and a span like captured text, and no model ever touched it. `Synthesized`
/// text was written from a pursuit; like a merge it has no corpus of its own
/// and names its sources through `artifact_sources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Passage,
    Captured,
    Merged,
    Synthesized,
    /// What a person typed about a file when they captured it. Source text
    /// like `Captured`, but owned by no window: it is *about* the document and
    /// is no line *of* it, so the two queries that treat a window-less row as
    /// debris from an older segmentation must leave it alone.
    Note,
}

impl Provenance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provenance::Passage => "passage",
            Provenance::Captured => "captured",
            Provenance::Merged => "merged",
            Provenance::Synthesized => "synthesized",
            Provenance::Note => "note",
        }
    }
    pub fn parse(s: &str) -> Provenance {
        match s {
            "passage" => Provenance::Passage,
            "merged" => Provenance::Merged,
            "synthesized" => Provenance::Synthesized,
            "note" => Provenance::Note,
            _ => Provenance::Captured,
        }
    }
    /// A model wrote this text. Such a row is never its own root: handing it
    /// back as one is how a paraphrase of a paraphrase reaches a prompt as an
    /// original. The test is "not source text" rather than "is merged", so the
    /// next value added defaults to safe rather than to wrong.
    pub fn is_model_written(&self) -> bool {
        matches!(self, Provenance::Merged | Provenance::Synthesized)
    }
    /// The text is the document's own, verbatim (`Passage`) or as the one
    /// synthesis rewrite of it (`Captured` — its own root by convention).
    pub fn is_source_text(&self) -> bool {
        !self.is_model_written()
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
    /// Which segmentation window produced this chunk. `None` for a merged
    /// artifact, which no window produced.
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
    /// For a synthesized artifact: the questions it was written for. Empty
    /// everywhere else.
    pub cues: Vec<String>,
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

/// An artifact written from a pursuit: what was asked, and what was engaged
/// with. Inserted through `insert_synthesized_artifact`.
#[derive(Debug, Clone)]
pub struct NewSynthesized {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
    /// The pursuit's queries: why this was written, shown on its page.
    pub cues: Vec<String>,
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
        cues: r
            .try_get::<Option<String>, _>("cues")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
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

        // The invariant stated above, checked rather than assumed. A merge over
        // passages rewrites the verbatim substrate into text that belongs to no
        // corpus and carries no span, and hides the wording someone captured
        // behind it. On the base this was written for, every one of the merge
        // path's root rows named a passage, silently, for as long as it ran.
        //
        // Here and not as a constraint on `artifact_sources`: the same table
        // carries a synthesis's passage sources, where naming a passage is
        // correct and intended.
        //
        // `Validation` and not `Internal` (`src/error.rs`): the caller sent a
        // root it may not merge, which is a refused request and not a broken
        // server.
        for root in &root_ids {
            let p: String = sqlx::query_scalar("SELECT provenance FROM artifacts WHERE id = ?")
                .bind(root.as_str())
                .fetch_one(&self.pool)
                .await?;
            if Provenance::parse(&p) != Provenance::Captured {
                return Err(crate::error::Error::Validation(format!(
                    "a merge root must be a captured artifact; {root} is {p}"
                )));
            }
        }

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
            cues: vec![],
        };
        sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, provenance, source_count, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, activation, activated_at)
             VALUES (?, NULL, 'merged', ?, 0, ?, NULL, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?, 1.0, ?)",
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
        .bind(c.created_at)
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

    /// Write an artifact generated from a pursuit. Like a merge it has no
    /// corpus of its own and names its sources through `artifact_sources`;
    /// unlike a merge it supersedes nothing — its sources stay active and
    /// keep ranking. `root_id` resolves through `roots_of`, so a generation
    /// written from another generation still names source text, and `via_id`
    /// keeps the chain reconstructible at any depth.
    pub async fn insert_synthesized_artifact(
        &self,
        new: &NewSynthesized,
        sources: &[String],
    ) -> Result<Chunk> {
        let resolved = self.roots_of(sources).await?;
        let root_ids: std::collections::BTreeSet<&String> = resolved.values().flatten().collect();
        let mut tx = self.pool.begin().await?;
        let created_at = now();
        let c = Chunk {
            id: new_id(),
            corpus_id: None,
            provenance: Provenance::Synthesized,
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
            cues: new.cues.clone(),
        };
        sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, provenance, source_count, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, activation, activated_at, cues)
             VALUES (?, NULL, 'synthesized', ?, 0, ?, NULL, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?, 1.0, ?, ?)",
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
        .bind(c.created_at)
        .bind(serde_json::to_string(&c.cues).unwrap_or_else(|_| "[]".into()))
        .execute(&mut *tx)
        .await?;
        for (via, roots) in &resolved {
            for root in roots {
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

    /// Generated artifacts still in results, newest first. What Ops lists.
    pub async fn synthesized_artifacts(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts
              WHERE provenance = 'synthesized' AND status = 'active' AND superseded_by IS NULL
              ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    pub async fn insert_artifacts(
        &self,
        corpus_id: &str,
        chunks: &[NewArtifact],
    ) -> Result<Vec<Chunk>> {
        self.insert_artifacts_with_provenance(corpus_id, chunks, Provenance::Captured)
            .await
    }

    /// `insert_artifacts`, saying what kind of row is being written. Capture at
    /// `off`/`earned` writes passages through this; everything else keeps the
    /// captured default.
    pub async fn insert_artifacts_with_provenance(
        &self,
        corpus_id: &str,
        chunks: &[NewArtifact],
        provenance: Provenance,
    ) -> Result<Vec<Chunk>> {
        let mut tx = self.pool.begin().await?;
        let mut out = Vec::with_capacity(chunks.len());
        for nc in chunks {
            let created_at = now();
            let c = Chunk {
                id: new_id(),
                corpus_id: Some(corpus_id.to_string()),
                provenance,
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
                cues: vec![],
            };
            sqlx::query(
                "INSERT INTO artifacts (id, corpus_id, provenance, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, activation, activated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, 1.0, ?)",
            )
            .bind(&c.id)
            .bind(&c.corpus_id)
            .bind(provenance.as_str())
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
            .bind(c.created_at)
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

    /// Every row a window owns, whatever its status, in ordinal order. The
    /// promotion path reads this to see what it is superseding and, on a
    /// retry, what it already wrote.
    pub async fn artifacts_for_segment(&self, corpus_id: &str, idx: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE corpus_id = ? AND segment_idx = ? ORDER BY ordinal",
        )
        .bind(corpus_id)
        .bind(idx)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    /// Set an artifact's activation outright, stamped `at`. Promotion uses it
    /// to hand a new artifact the access its passages earned; everything else
    /// goes through `bump_activation`, which adds.
    pub async fn set_activation(&self, id: &str, value: f64, at: i64) -> Result<()> {
        sqlx::query("UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?")
            .bind(value)
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Has a person ever said this artifact was the answer? A `hit` verdict
    /// naming it, on any recorded search.
    pub async fn artifact_confirmed(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM search_events WHERE verdict = 'hit' AND expect_id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?
            > 0)
    }

    /// The rows for these ids, in no particular order; ids that name nothing
    /// are simply absent. One query for a page of hits.
    pub async fn artifacts_by_ids(&self, ids: &[String]) -> Result<Vec<Chunk>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let holes = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT * FROM artifacts WHERE id IN ({holes})"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(row_to_artifact)
            .collect())
    }

    /// The caveats of many artifacts in one query, keyed by id.
    ///
    /// For the ask path, which needs the caveats of every hit and nothing else
    /// from the row: one lookup per hit is cheap, but a follow-up round packs
    /// the merged list again, and a dozen sequential round trips in front of
    /// every model call adds up. An id with no row — deleted since it was
    /// retrieved — is simply absent from the map.
    pub async fn caveats_for(&self, ids: &[String]) -> Result<HashMap<String, Vec<String>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let holes = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, caveats FROM artifacts WHERE id IN ({holes})"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    r.get::<Option<String>, _>("caveats")
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                )
            })
            .collect())
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

    /// The nearest *active* artifact either side of `ordinal` in the same
    /// corpus — not the rows at `ordinal ± 1`.
    ///
    /// The answer to a question is often the paragraph after the one that
    /// matched, and `ordinal` is a continuous per-corpus sequence, which is
    /// what makes this a lookup instead of a search. But after a promotion the
    /// sequence interleaves superseded passages with what replaced them, and
    /// "the row next door" is then often a hidden one; the reader wants the
    /// next thing still in the document. An edge returns the one side that
    /// exists; `status = 'active'` keeps deprecated and superseded artifacts
    /// out, exactly as an ordinary search would.
    pub async fn adjacent_artifacts(&self, corpus_id: &str, ordinal: i64) -> Result<Vec<Chunk>> {
        let before = sqlx::query(
            "SELECT * FROM artifacts
             WHERE corpus_id = ? AND ordinal < ? AND status = 'active'
             ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(corpus_id)
        .bind(ordinal)
        .fetch_optional(&self.pool)
        .await?;
        let after = sqlx::query(
            "SELECT * FROM artifacts
             WHERE corpus_id = ? AND ordinal > ? AND status = 'active'
             ORDER BY ordinal ASC LIMIT 1",
        )
        .bind(corpus_id)
        .bind(ordinal)
        .fetch_optional(&self.pool)
        .await?;
        Ok(before
            .iter()
            .chain(after.iter())
            .map(row_to_artifact)
            .collect())
    }

    /// For each of `ids`, the next *active* artifact in the same document —
    /// one read for a whole result list rather than one per row.
    ///
    /// What the rail needs to say "this one continues". Absent means the
    /// artifact ends its document, or belongs to none: a merged artifact has no
    /// `corpus_id` and therefore no reading order to continue in, and the join
    /// drops it rather than reaching into another document's ordinals.
    ///
    /// A correlated subquery rather than a second round trip per hit. The rail
    /// is drawn on every keystroke of a search-while-typing box, and N+1 reads
    /// down a ten-row list is the shape that turns a lookup into a cost.
    pub async fn continuations_of(&self, ids: &[String]) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let holes = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT a.id AS id,
                    (SELECT n.id FROM artifacts n
                      WHERE n.corpus_id = a.corpus_id
                        AND n.ordinal > a.ordinal
                        AND n.status = 'active'
                      ORDER BY n.ordinal ASC LIMIT 1) AS next_id
             FROM artifacts a
             WHERE a.id IN ({holes}) AND a.corpus_id IS NOT NULL"
        )));
        for id in ids {
            q = q.bind(id);
        }
        Ok(q.fetch_all(&self.pool)
            .await?
            .iter()
            .filter_map(|r| {
                r.get::<Option<String>, _>("next_id")
                    .map(|next| (r.get::<String, _>("id"), next))
            })
            .collect())
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
        let pending: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM artifacts WHERE corpus_id = ? AND embed_state = 'pending'",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        let armed = self
            .control
            .targets_with_jobs(
                &self.subject,
                crate::store::jobs::Stage::Embed,
                &pending,
                &["pending", "running", "failed"],
                None,
            )
            .await?;
        Ok(pending.iter().all(|id| armed.contains(id)))
    }

    /// Put every chunk of a source back in the embed queue's path. Re-embedding
    /// only happens for rows that say they still need it, so asking for it has
    /// to say so first.
    ///
    /// The revision bump is what makes this safe to run while a worker is
    /// mid-batch on the same source: that worker's `mark_embedded` no longer
    /// matches, so it cannot clear the pending state this just set.
    ///
    /// `updated_at` is deliberately left alone. It answers whether the text on
    /// screen is the text that was captured, and re-embedding changes no text —
    /// a model change or a `--reindex` would otherwise stamp every artifact in
    /// the corpus as edited today, which is the one thing the column is there
    /// to deny.
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
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
            )
            .bind(title)
            .bind(super::now())
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
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
            )
            .bind(text)
            .bind(super::now())
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
    /// Chunks with no window at all are included: leaving them out would append
    /// the new segmentation beside the old one instead of replacing it. They
    /// are swept by whichever window writes first, and there are none left by
    /// the second.
    ///
    /// Except a `note`, which is window-less because it belongs to no window
    /// and not because it is left over from an older split. Sweeping it made
    /// the first window see the corpus as already written and skip every
    /// passage, so a captured file with an annotation was never chunked at
    /// all.
    pub async fn artifact_ids_for_segment(
        &self,
        corpus_id: &str,
        segment_idx: i64,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM artifacts WHERE corpus_id = ? AND provenance != 'note'
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

    /// Every artifact id. What the store-drift heal compares against Qdrant.
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
    ///
    /// Walked in windows behind a cursor, rather than always from the oldest
    /// artifact forward. The "never asked" test lives in the other database
    /// now, so it cannot be a `WHERE` clause any more and the `LIMIT` has to be
    /// applied after it — which means a fixed window over the *oldest* rows,
    /// and on any base with a history that window is entirely artifacts asked
    /// about long ago. Every pass would filter all of them out, return nothing,
    /// and look at exactly the same rows an hour later: a backstop permanently
    /// blind to everything behind the window, which is everything that has
    /// been captured since. The cursor is what makes the pass move.
    ///
    /// Reaching the end clears it, so the walk wraps and nothing is out of
    /// reach for longer than one lap. It advances to the end of the window
    /// examined and not to the last id returned, so a pass always makes
    /// progress even when the whole window was already armed; a window holding
    /// more unarmed artifacts than the caller asked for leaves the remainder
    /// to the next lap, which is what a backstop is for.
    pub async fn list_unrelated_artifact_ids(&self, limit: usize) -> Result<Vec<String>> {
        const CURSOR: &str = "repair.relate_backstop_after";
        // Over-fetched against the caller's limit, for the reason it always
        // was: an unknown share of any window is already armed.
        let window = (limit as i64).saturating_mul(8).max(1);
        let after = self.meta_get(CURSOR).await?.and_then(|s| {
            let (at, id) = s.split_once(':')?;
            Some((at.parse::<i64>().ok()?, id.to_string()))
        });
        let (ts, from) = match &after {
            Some((ts, id)) => (*ts, id.as_str()),
            None => (i64::MIN, ""),
        };
        let rows = sqlx::query(
            "SELECT a.id, a.created_at FROM artifacts a
              WHERE a.status = 'active' AND a.superseded_by IS NULL
                AND a.embed_state = 'embedded'
                AND a.provenance <> 'passage'
                AND (a.created_at > ? OR (a.created_at = ? AND a.id > ?))
              ORDER BY a.created_at ASC, a.id ASC
              LIMIT ?",
        )
        .bind(ts)
        .bind(ts)
        .bind(from)
        .bind(window)
        .fetch_all(&self.pool)
        .await?;
        // A short window is the end of the walk: start again from the oldest
        // next time, so an artifact that lost its unit while the cursor was
        // already past it is found on the next lap.
        let next = match rows.last() {
            Some(r) if (rows.len() as i64) == window => {
                format!(
                    "{}:{}",
                    r.get::<i64, _>("created_at"),
                    r.get::<String, _>("id")
                )
            }
            _ => String::new(),
        };
        self.meta_set(CURSOR, &next).await?;
        let candidates: Vec<String> = rows.iter().map(|r| r.get::<String, _>("id")).collect();
        let armed = self
            .control
            .targets_with_jobs(
                &self.subject,
                crate::store::jobs::Stage::Relate,
                &candidates,
                &[],
                None,
            )
            .await?;
        Ok(candidates
            .into_iter()
            .filter(|id| !armed.contains(id))
            .take(limit)
            .collect())
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

    /// One passage in a corpus of its own, for the root-provenance cases.
    async fn a_passage(s: &Store) -> String {
        let src = s.insert_corpus("skript", "web", None).await.unwrap();
        let mut p = nc(0, "Spuren sind materielle Veraenderungen.");
        p.segment_idx = Some(0);
        s.insert_artifacts_with_provenance(&src.id, &[p], Provenance::Passage)
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn a_merge_whose_root_is_a_passage_is_refused() {
        // The invariant `insert_merged_artifact` documents and the live base
        // violated in every one of its 135 merge-lineage rows. A merge over
        // passages rewrites the verbatim substrate into text that belongs to no
        // corpus and carries no span, and hides the wording someone captured
        // behind it — the outcome `schema.sql` and the ROADMAP's fidelity rule
        // exist to prevent.
        let s = Store::memory().await.unwrap();
        let root = a_passage(&s).await;

        let refused = s
            .insert_merged_artifact(
                &NewMerged {
                    title: Some("merged".into()),
                    text: "rewritten".into(),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[root],
            )
            .await;

        assert!(refused.is_err(), "a passage is not a merge root");
    }

    #[tokio::test]
    async fn a_synthesized_artifact_may_still_name_passage_sources() {
        // Why the check is on the merge path and not on `artifact_sources`. A
        // synthesis draws on passages by design, and eleven rows in the live
        // base are exactly that; a table constraint would break it.
        let s = Store::memory().await.unwrap();
        let root = a_passage(&s).await;

        let made = s
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "Zusammenfassung".into(),
                    title: Some("Spurenkunde".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                &[root],
            )
            .await;

        assert!(made.is_ok(), "synthesis over passages is what synthesis is");
    }

    /// One query for every hit's caveats: each id maps to its own list, an id
    /// with no row is absent rather than an error, and nothing asked for is
    /// confused with anything else in the table.
    #[tokio::test]
    async fn caveats_for_reads_many_artifacts_in_one_query() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut a = nc(0, "one");
        a.caveats = vec!["destroys everything on the device".into()];
        let b = nc(1, "two");
        let c = nc(2, "three");
        let made = s.insert_artifacts(&src.id, &[a, b, c]).await.unwrap();

        let ids = vec![made[0].id.clone(), made[1].id.clone(), "gone".to_string()];
        let got = s.caveats_for(&ids).await.unwrap();
        assert_eq!(
            got.get(&made[0].id).map(Vec::as_slice),
            Some(&["destroys everything on the device".to_string()][..])
        );
        assert_eq!(got.get(&made[1].id).map(Vec::len), Some(0));
        assert!(
            !got.contains_key("gone"),
            "a missing row must be absent, not invented"
        );
        assert!(
            !got.contains_key(&made[2].id),
            "an id not asked for came back"
        );
        assert!(s.caveats_for(&[]).await.unwrap().is_empty());
    }

    /// The answer is often in the artifact next to the one that matched, and
    /// `ordinal` is already a continuous per-corpus sequence, so this is a
    /// lookup rather than a search.
    #[tokio::test]
    async fn adjacent_artifacts_returns_the_ordinals_either_side() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..5).map(|i| nc(i, &format!("chunk {i}"))).collect();
        s.insert_artifacts(&src.id, &new).await.unwrap();

        let got = s.adjacent_artifacts(&src.id, 2).await.unwrap();
        let ordinals: Vec<i64> = got.iter().map(|c| c.ordinal).collect();
        assert_eq!(ordinals, vec![1, 3]);
    }

    /// The first artifact has no left neighbour, and asking for ordinal -1 must
    /// return the one row that exists rather than an error or nothing.
    #[tokio::test]
    async fn adjacent_artifacts_at_the_edge_returns_only_the_side_that_exists() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..5).map(|i| nc(i, &format!("chunk {i}"))).collect();
        s.insert_artifacts(&src.id, &new).await.unwrap();

        let got = s.adjacent_artifacts(&src.id, 0).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ordinal, 1);
    }

    /// Reaching sideways must not resurrect what the lifecycle took out of
    /// results: a deprecated neighbour is not an answer.
    #[tokio::test]
    async fn adjacent_artifacts_skips_a_neighbour_that_is_not_active() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..3).map(|i| nc(i, &format!("chunk {i}"))).collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let got = s.adjacent_artifacts(&src.id, 1).await.unwrap();
        let ordinals: Vec<i64> = got.iter().map(|c| c.ordinal).collect();
        assert_eq!(ordinals, vec![2]);
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
        s.set_corpus_coverage(&src.id, Some(0.42)).await.unwrap();
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
    async fn re_embedding_a_corpus_is_not_an_edit() {
        // `updated_at` answers whether the text on screen is the text that was
        // captured. Re-embedding changes no text — it is a model change or a
        // `--reindex` — so stamping it here would mark every artifact in the
        // corpus as edited today, which is the one thing the column exists to
        // deny.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let c = s.insert_artifacts(&src.id, &[nc(0, "x")]).await.unwrap()[0].clone();

        s.reset_embed_state(&src.id).await.unwrap();

        let stamp: i64 = sqlx::query_scalar("SELECT updated_at FROM artifacts WHERE id = ?")
            .bind(&c.id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(stamp, 0, "a re-embed reported itself as an edit");
        let got = s.artifacts_for_corpus(&src.id).await.unwrap();
        assert_eq!(
            got[0].embed_state,
            EmbedState::Pending,
            "and it did re-queue"
        );
    }

    #[tokio::test]
    async fn editing_an_artifact_stamps_when_it_changed() {
        // The one question the base could not previously answer about itself.
        // `created_at` says when it arrived and `last_verified_at` says when
        // someone vouched for it; neither says whether the text on screen is
        // the text that was captured.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let c = s
            .insert_artifacts(&src.id, &[nc(0, "before")])
            .await
            .unwrap()
            .remove(0);

        let before: i64 = sqlx::query_scalar("SELECT updated_at FROM artifacts WHERE id = ?")
            .bind(&c.id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(before, 0, "a fresh row has never been edited");

        s.update_artifact_text(&c.id, "after").await.unwrap();

        let after: i64 = sqlx::query_scalar("SELECT updated_at FROM artifacts WHERE id = ?")
            .bind(&c.id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert!(after > 0, "an edit says when it happened");
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

    #[test]
    fn provenance_round_trips_all_four_values_and_unknown_reads_as_captured() {
        for p in [
            Provenance::Passage,
            Provenance::Captured,
            Provenance::Merged,
            Provenance::Synthesized,
        ] {
            assert_eq!(Provenance::parse(p.as_str()), p);
        }
        assert_eq!(Provenance::parse("whatever"), Provenance::Captured);
        assert!(Provenance::Merged.is_model_written());
        assert!(Provenance::Synthesized.is_model_written());
        assert!(Provenance::Passage.is_source_text());
        assert!(Provenance::Captured.is_source_text());
        assert!(!Provenance::Passage.is_model_written());
    }

    #[tokio::test]
    async fn a_passage_is_inserted_as_one_and_read_back_as_one() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts_with_provenance(&src.id, &[nc(0, "verbatim")], Provenance::Passage)
            .await
            .unwrap();
        assert_eq!(made[0].provenance, Provenance::Passage);
        let read = s.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(read.provenance, Provenance::Passage);
        // The old entry point still writes captured rows.
        let cap = s
            .insert_artifacts(&src.id, &[nc(1, "captured")])
            .await
            .unwrap();
        assert_eq!(cap[0].provenance, Provenance::Captured);
    }

    #[tokio::test]
    async fn artifacts_by_ids_returns_the_rows_asked_for_and_skips_the_missing() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b"), nc(2, "c")])
            .await
            .unwrap();
        let got = s
            .artifacts_by_ids(&[made[2].id.clone(), "gone".into(), made[0].id.clone()])
            .await
            .unwrap();
        let mut texts: Vec<&str> = got.iter().map(|c| c.text.as_str()).collect();
        texts.sort_unstable();
        assert_eq!(texts, vec!["a", "c"]);
    }

    #[tokio::test]
    async fn the_relate_backstop_never_lists_a_passage() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let p = s
            .insert_artifacts_with_provenance(&src.id, &[nc(0, "p")], Provenance::Passage)
            .await
            .unwrap();
        let c = s.insert_artifacts(&src.id, &[nc(1, "c")]).await.unwrap();
        for id in [&p[0].id, &c[0].id] {
            s.mark_embedded(id, "fake", 0).await.unwrap();
        }
        let ids = s.list_unrelated_artifact_ids(10).await.unwrap();
        assert_eq!(ids, vec![c[0].id.clone()]);
    }

    /// The backstop has to be able to see past its own window.
    ///
    /// A relate row survives its completion — that is the whole mechanism, "no
    /// job at all" meaning "never asked" — so on a base with any history the
    /// oldest artifacts are all armed. A fixed window over them comes back
    /// empty every pass, for ever, and everything behind it is out of reach.
    #[tokio::test]
    async fn the_relate_backstop_walks_past_a_window_that_is_already_armed() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made: Vec<_> = (0..12).map(|i| nc(i, "text")).collect();
        for a in s.insert_artifacts(&src.id, &made).await.unwrap() {
            s.mark_embedded(&a.id, "fake", 0).await.unwrap();
        }
        // In the order the walk takes them, so "the first window" means
        // something whatever ids were minted.
        let ordered: Vec<String> =
            sqlx::query_scalar("SELECT id FROM artifacts ORDER BY created_at, id")
                .fetch_all(&s.pool)
                .await
                .unwrap();
        let (behind, armed) = ordered.split_last().unwrap();
        for id in armed {
            s.enqueue(crate::store::jobs::Stage::Relate, "artifact", id)
                .await
                .unwrap();
        }

        // A limit of one is a window of eight, and all eight are armed.
        assert!(
            s.list_unrelated_artifact_ids(1).await.unwrap().is_empty(),
            "the first window holds nothing to arm"
        );
        assert_eq!(
            s.list_unrelated_artifact_ids(1).await.unwrap(),
            vec![behind.clone()],
            "the walk never moved past the window it started in"
        );
    }

    /// And the walk wraps: an artifact that loses its unit while the cursor is
    /// already past it is found on the next lap rather than never.
    #[tokio::test]
    async fn the_relate_backstop_starts_over_when_it_runs_out() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made: Vec<_> = (0..3).map(|i| nc(i, "text")).collect();
        let ids: Vec<String> = s
            .insert_artifacts(&src.id, &made)
            .await
            .unwrap()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        for id in &ids {
            s.mark_embedded(id, "fake", 0).await.unwrap();
            s.enqueue(crate::store::jobs::Stage::Relate, "artifact", id)
                .await
                .unwrap();
        }
        assert!(s.list_unrelated_artifact_ids(8).await.unwrap().is_empty());

        // The arming that went missing, behind a cursor that has already been
        // everywhere.
        sqlx::query("DELETE FROM jobs WHERE target_id = ?")
            .bind(&ids[0])
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(
            s.list_unrelated_artifact_ids(8).await.unwrap(),
            vec![ids[0].clone()],
            "the walk did not start over"
        );
    }

    /// The rail says a hit continues, and the whole list has to be answered in
    /// one read rather than one per row.
    #[tokio::test]
    async fn continuations_of_names_the_next_active_artifact_for_a_whole_list() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b"), nc(2, "c")])
            .await
            .unwrap();

        let ids = vec![made[0].id.clone(), made[1].id.clone()];
        let got = s.continuations_of(&ids).await.unwrap();
        assert_eq!(got.get(&made[0].id), Some(&made[1].id));
        assert_eq!(got.get(&made[1].id), Some(&made[2].id));
    }

    /// The last passage of a document continues nowhere, and the rail must say
    /// nothing rather than offer a link to the end of the list.
    #[tokio::test]
    async fn continuations_of_omits_the_last_artifact_of_a_document() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();

        let got = s.continuations_of(&[made[1].id.clone()]).await.unwrap();
        assert!(
            !got.contains_key(&made[1].id),
            "the end of a document was given a continuation"
        );
    }

    /// The same lifecycle rule as everywhere else on this path: what results
    /// hide is not what the document continues into.
    #[tokio::test]
    async fn continuations_of_steps_over_a_row_that_is_not_active() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b"), nc(2, "c")])
            .await
            .unwrap();
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let got = s.continuations_of(&[made[0].id.clone()]).await.unwrap();
        assert_eq!(got.get(&made[0].id), Some(&made[2].id));
    }

    /// A merged artifact belongs to no corpus, so it has no reading order to
    /// continue in. Asking must not invent one out of another document's rows.
    #[tokio::test]
    async fn continuations_of_says_nothing_about_a_merged_artifact() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        s.insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        let merged = s
            .insert_merged_artifact(
                &NewMerged {
                    text: "merged".into(),
                    title: Some("m".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[],
            )
            .await
            .unwrap();

        let got = s
            .continuations_of(std::slice::from_ref(&merged.id))
            .await
            .unwrap();
        assert!(!got.contains_key(&merged.id));
        assert!(s.continuations_of(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn adjacent_artifacts_steps_over_a_superseded_row_to_the_next_active_one() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b"), nc(2, "c"), nc(3, "d")])
            .await
            .unwrap();
        // Hide b and c; a's next active neighbour is d.
        s.set_superseded_by(&made[1].id, Some(&made[3].id))
            .await
            .unwrap();
        s.set_superseded_by(&made[2].id, Some(&made[3].id))
            .await
            .unwrap();
        let got = s.adjacent_artifacts(&src.id, 0).await.unwrap();
        assert_eq!(
            got.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["d"]
        );
        let got = s.adjacent_artifacts(&src.id, 3).await.unwrap();
        assert_eq!(
            got.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    #[tokio::test]
    async fn set_activation_writes_value_and_stamp_and_artifacts_for_segment_reads_every_status() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut a = nc(0, "a");
        a.segment_idx = Some(3);
        let mut b = nc(1, "b");
        b.segment_idx = Some(3);
        let mut c = nc(2, "c");
        c.segment_idx = Some(4);
        let made = s.insert_artifacts(&src.id, &[a, b, c]).await.unwrap();
        s.set_superseded_by(&made[1].id, Some(&made[0].id))
            .await
            .unwrap();
        let seg = s.artifacts_for_segment(&src.id, 3).await.unwrap();
        assert_eq!(
            seg.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );

        s.set_activation(&made[0].id, 7.25, 99).await.unwrap();
        let act = s
            .activation_of(std::slice::from_ref(&made[0].id))
            .await
            .unwrap();
        assert_eq!(act[&made[0].id], (7.25, 99));
    }

    #[tokio::test]
    async fn artifact_confirmed_reads_a_hit_verdict_naming_it() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s.insert_artifacts(&src.id, &[nc(0, "a")]).await.unwrap();
        assert!(!s.artifact_confirmed(&made[0].id).await.unwrap());
        let ev = s
            .record_search(
                crate::store::feedback::NewEvent {
                    query: "q".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        s.judge_hit(&ev, &made[0].id).await.unwrap();
        assert!(s.artifact_confirmed(&made[0].id).await.unwrap());
    }

    #[tokio::test]
    async fn a_synthesized_artifact_names_source_text_as_roots_at_any_depth() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        let gen1 = s
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "written from a and b".into(),
                    title: Some("G1".into()),
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec!["how do I a and b".into()],
                },
                &[made[0].id.clone(), made[1].id.clone()],
            )
            .await
            .unwrap();
        assert_eq!(gen1.provenance, Provenance::Synthesized);
        assert!(gen1.corpus_id.is_none());
        let read = s.get_artifact(&gen1.id).await.unwrap();
        assert_eq!(read.cues, vec!["how do I a and b".to_string()]);
        // Its sources stay active: a generation supersedes nothing.
        assert!(s.get_artifact(&made[0].id).await.unwrap().in_results());
        // A generation written from the generation: roots are still a and b,
        // reached through G1.
        let gen2 = s
            .insert_synthesized_artifact(
                &NewSynthesized {
                    text: "written from G1".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                    cues: vec![],
                },
                std::slice::from_ref(&gen1.id),
            )
            .await
            .unwrap();
        let roots = s.roots_of(std::slice::from_ref(&gen2.id)).await.unwrap();
        let mut got = roots[&gen2.id].clone();
        got.sort();
        let mut want = vec![made[0].id.clone(), made[1].id.clone()];
        want.sort();
        assert_eq!(got, want, "roots must be captured text, never a generation");
        let via = s.sources_with_via(&gen2.id).await.unwrap();
        assert!(
            via.iter()
                .all(|(_, v)| v.as_deref() == Some(gen1.id.as_str())),
            "{via:?}"
        );
        assert_eq!(s.synthesized_artifacts(10).await.unwrap().len(), 2);
    }
}
