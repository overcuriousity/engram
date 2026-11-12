use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;
use std::collections::HashMap;

/// One string for the drift heal and the backup count, so they agree on what an artifact is.
pub(crate) const ALL_ARTIFACT_IDS: &str = "SELECT id FROM artifacts";
/// A backup's `embedded` count is this query, for the same reason.
pub(crate) const EMBEDDED_ARTIFACT_IDS: &str =
    "SELECT id FROM artifacts WHERE embed_state = 'embedded' AND reaped_at IS NULL";

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

#[derive(Debug, Clone)]
pub struct Grave {
    pub id: String,
    pub title: Option<String>,
    pub reaped_at: i64,
    pub reason: Option<String>,
}

/// Where an artifact stands: still current, flagged stale with no named
/// replacement, or hidden in favour of a specific `superseded_by` artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Active,
    Deprecated,
    Superseded,
    /// Deleted by a person and restorable for 30 days; out of every read.
    Trashed,
}

impl ArtifactStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactStatus::Active => "active",
            ArtifactStatus::Deprecated => "deprecated",
            ArtifactStatus::Superseded => "superseded",
            ArtifactStatus::Trashed => "trashed",
        }
    }
    pub fn parse(s: &str) -> ArtifactStatus {
        match s {
            "deprecated" => ArtifactStatus::Deprecated,
            "superseded" => ArtifactStatus::Superseded,
            "trashed" => ArtifactStatus::Trashed,
            _ => ArtifactStatus::Active,
        }
    }
}

/// Branch on this, never on `corpus_id.is_none()`. `Captured` and `Passage` have a corpus
/// and span; `Merged` and `Synthesized` have neither and name sources via `artifact_sources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Passage,
    Captured,
    Merged,
    Synthesized,
    /// Owned by no window, so queries treating window-less rows as debris must spare it.
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
    /// Never its own root. "Not source text" rather than "is merged", so a new variant is safe.
    pub fn is_model_written(&self) -> bool {
        matches!(self, Provenance::Merged | Provenance::Synthesized)
    }
    pub fn is_source_text(&self) -> bool {
        !self.is_model_written()
    }
    /// A `Passage` title is its section's heading and a `Note` has none; everything else was
    /// named by the model that wrote it. Not the same split as `is_model_written`.
    pub fn names_its_own_text(&self) -> bool {
        !matches!(self, Provenance::Passage | Provenance::Note)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CorpusSpan {
    pub start_line: i64,
    pub end_line: i64,
    /// Absent is `Unplaced`, so an old span fails towards leaving the verbatim passage standing.
    #[serde(default)]
    pub source: SpanSource,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpanSource {
    /// The strongest: the only one that survives a paraphrase reflowing hard-wrapped source.
    Located,
    /// Weak, but still a claim about which lines.
    Claimed,
    /// Nothing placed it, so the span is the whole window and distinguishes no artifact in it.
    #[default]
    Unplaced,
}

impl CorpusSpan {
    pub fn located(start_line: i64, end_line: i64) -> Self {
        Self {
            start_line,
            end_line,
            source: SpanSource::Located,
        }
    }

    pub fn claimed(start_line: i64, end_line: i64) -> Self {
        Self {
            start_line,
            end_line,
            source: SpanSource::Claimed,
        }
    }

    pub fn unplaced(start_line: i64, end_line: i64) -> Self {
        Self {
            start_line,
            end_line,
            source: SpanSource::Unplaced,
        }
    }

    /// Whether the span can corroborate a claim, rather than being true of the whole window.
    pub fn places_the_artifact(&self) -> bool {
        self.source != SpanSource::Unplaced
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Chunk {
    pub id: String,
    /// Branch on `provenance`, not on this: see `Provenance`.
    pub corpus_id: Option<String>,
    pub provenance: Provenance,
    /// Zero for a captured artifact.
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
    /// Internal bookkeeping between the editor and the embed job.
    #[serde(skip)]
    pub embed_rev: i64,
    pub segment_idx: Option<i64>,
    /// Verification failures. Empty means every check passed.
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    /// Set by consolidation; the artifact stays readable and is only kept out of ranking.
    pub superseded_by: Option<String>,
    /// Not embedded: changing what vectors are built from is the evaluation harness's decision.
    pub caveats: Vec<String>,
    /// Kept in sync with `superseded_by` by `set_superseded_by`.
    pub status: ArtifactStatus,
    /// Defaults to `created_at`; this is what search ranking decays against.
    pub last_verified_at: Option<i64>,
    /// For a synthesized artifact: the questions it was written for.
    pub cues: Vec<String>,
    /// Cleared on the way back to `active`. What the reap sweep's age rule reads.
    pub retired_at: Option<i64>,
    /// A stub, links intact and text gone, never a candidate again.
    pub reaped_at: Option<i64>,
}

/// The one spelling of "active and not hidden" every consolidation decision gates on;
/// free so `ui::ArtifactDetail` can ask it too.
pub fn in_results(status: ArtifactStatus, superseded_by: Option<&str>) -> bool {
    status == ArtifactStatus::Active && superseded_by.is_none()
}

impl Chunk {
    pub fn in_results(&self) -> bool {
        in_results(self.status, self.superseded_by.as_deref())
    }
}

/// Not `NewArtifact`: optional corpus and span fields would invite a merge to claim them.
#[derive(Debug, Clone)]
pub struct NewMerged {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
}

/// Test-only: in production every field is a decision, so a new field must break every call site.
#[cfg(test)]
impl Default for NewMerged {
    fn default() -> Self {
        Self {
            text: String::new(),
            title: None,
            category: None,
            tags: vec![],
            caveats: vec![],
        }
    }
}

/// A struct, not seven positional parameters.
struct Sourced<'a> {
    provenance: Provenance,
    text: &'a str,
    title: Option<&'a String>,
    category: Option<&'a String>,
    tags: &'a [String],
    caveats: &'a [String],
    /// Empty for a merge, which is asked nothing.
    cues: &'a [String],
}

#[derive(Debug, Clone)]
pub struct NewSynthesized {
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub caveats: Vec<String>,
    pub cues: Vec<String>,
}

/// Test-only, not a `derive`: a blanket `Default` would stop a new field breaking call sites.
#[cfg(test)]
impl Default for NewArtifact {
    fn default() -> Self {
        Self {
            ordinal: 0,
            text: String::new(),
            corpus_span: None,
            title: None,
            category: None,
            tags: vec![],
            segment_idx: None,
            caveats: vec![],
        }
    }
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

#[derive(sqlx::FromRow)]
pub(crate) struct ArtifactRow {
    pub id: Option<String>,
    pub corpus_id: Option<String>,
    pub provenance: String,
    pub source_count: i64,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub lifecycle_dirty: i64,
    pub ordinal: i64,
    pub text: String,
    pub corpus_span: Option<String>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: String,
    pub embed_state: String,
    pub embed_model: Option<String>,
    pub created_at: i64,
    pub embed_rev: i64,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub updated_at: i64,
    pub segment_idx: Option<i64>,
    pub flags: Option<String>,
    pub flag_detail: Option<String>,
    pub superseded_by: Option<String>,
    pub caveats: String,
    pub status: String,
    pub last_verified_at: Option<i64>,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub activation: f64,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub activated_at: i64,
    pub cues: String,
    pub retired_at: Option<i64>,
    pub reaped_at: Option<i64>,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub trashed_at: Option<i64>,
    #[allow(dead_code, reason = "filled by query_as!, not carried by Chunk")]
    pub trashed_from: Option<String>,
}

impl From<ArtifactRow> for Chunk {
    fn from(r: ArtifactRow) -> Chunk {
        let json = |s: &str| serde_json::from_str(s).unwrap_or_default();
        Chunk {
            id: r.id.unwrap_or_default(),
            corpus_id: r.corpus_id,
            provenance: Provenance::parse(&r.provenance),
            source_count: r.source_count,
            ordinal: r.ordinal,
            text: r.text,
            corpus_span: r.corpus_span.and_then(|s| serde_json::from_str(&s).ok()),
            title: r.title,
            category: r.category,
            tags: json(&r.tags),
            embed_state: EmbedState::parse(&r.embed_state),
            embed_model: r.embed_model,
            created_at: r.created_at,
            embed_rev: r.embed_rev,
            segment_idx: r.segment_idx,
            flags: r.flags.as_deref().map(json).unwrap_or_default(),
            flag_detail: r.flag_detail,
            superseded_by: r.superseded_by,
            caveats: json(&r.caveats),
            status: ArtifactStatus::parse(&r.status),
            last_verified_at: r.last_verified_at,
            cues: json(&r.cues),
            retired_at: r.retired_at,
            reaped_at: r.reaped_at,
        }
    }
}

pub(crate) fn row_to_artifact(r: ArtifactRow) -> Chunk {
    Chunk::from(r)
}

impl Store {
    /// One transaction, so no crash leaves an artifact without lineage. Sources are flattened to
    /// captured roots, so `root_id` only names captured artifacts; `via_id` keeps the chain.
    async fn insert_sourced_artifact(&self, new: Sourced<'_>, sources: &[String]) -> Result<Chunk> {
        let Sourced {
            provenance,
            text,
            title,
            category,
            tags,
            caveats,
            cues,
        } = new;
        // Resolved before the transaction opens: holding a write lock across a read buys nothing.
        let resolved = self.roots_of(sources).await?;
        // The roots about to be written, deduped as the table keys them: M2 = merge(M1(a,b), c)
        // must count three, or `merged_missing_a_source` never fires for it.
        let root_ids: std::collections::BTreeSet<&String> = resolved.values().flatten().collect();

        // A merge's roots must be captured, not passages: checked here, not as a table constraint,
        // since a synthesis may cite passages. `Validation`: a refused request, not a fault.
        if provenance == Provenance::Merged {
            for root in &root_ids {
                let p: String = sqlx::query_scalar!(
                    "SELECT provenance FROM artifacts WHERE id = ?",
                    root.as_str()
                )
                .fetch_one(&self.pool)
                .await?;
                if Provenance::parse(&p) != Provenance::Captured {
                    return Err(crate::error::Error::Validation(format!(
                        "a merge root must be a captured artifact; {root} is {p}"
                    )));
                }
            }
        }

        let mut tx = self.pool.begin().await?;
        let created_at = now();
        let c = Chunk {
            id: new_id(),
            corpus_id: None,
            provenance,
            source_count: root_ids.len() as i64,
            ordinal: 0,
            text: text.to_string(),
            corpus_span: None,
            title: title.cloned(),
            category: category.cloned(),
            tags: tags.to_vec(),
            embed_state: EmbedState::Pending,
            embed_model: None,
            created_at,
            embed_rev: 0,
            segment_idx: None,
            flags: vec![],
            flag_detail: None,
            superseded_by: None,
            caveats: caveats.to_vec(),
            status: ArtifactStatus::Active,
            last_verified_at: Some(created_at),
            cues: cues.to_vec(),
            retired_at: None,
            reaped_at: None,
        };
        sqlx::query!(
            "INSERT INTO artifacts (id, corpus_id, provenance, source_count, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, activation, activated_at, cues)
             VALUES (?, NULL, ?, ?, 0, ?, NULL, ?, ?, ?, ?, NULL, ?, NULL, ?, ?, ?, 1.0, ?, ?)",
            &c.id,
            c.provenance.as_str(),
            c.source_count,
            &c.text,
            &c.title,
            &c.category,
            serde_json::to_string(&c.tags).unwrap_or_else(|_| "[]".into()),
            c.embed_state.as_str(),
            c.created_at,
            serde_json::to_string(&c.caveats).unwrap_or_else(|_| "[]".into()),
            c.status.as_str(),
            c.last_verified_at,
            c.created_at,
            serde_json::to_string(&c.cues).unwrap_or_else(|_| "[]".into())
        )
            .execute(&mut *tx)
        .await?;

        for (via, roots) in &resolved {
            for root in roots {
                // OR IGNORE: merging M(a,b) with `a` itself names `a` twice.
                sqlx::query!(
                    "INSERT OR IGNORE INTO artifact_sources (child_id, root_id, via_id, created_at)
                     VALUES (?, ?, ?, ?)",
                    &c.id,
                    root,
                    via,
                    created_at
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(c)
    }

    /// Every root must be captured; `insert_sourced_artifact` refuses otherwise.
    pub async fn insert_merged_artifact(
        &self,
        new: &NewMerged,
        sources: &[String],
    ) -> Result<Chunk> {
        self.insert_sourced_artifact(
            Sourced {
                provenance: Provenance::Merged,
                text: &new.text,
                title: new.title.as_ref(),
                category: new.category.as_ref(),
                tags: &new.tags,
                caveats: &new.caveats,
                cues: &[],
            },
            sources,
        )
        .await
    }

    /// Unlike a merge it supersedes nothing, and its roots need not be captured.
    pub async fn insert_synthesized_artifact(
        &self,
        new: &NewSynthesized,
        sources: &[String],
    ) -> Result<Chunk> {
        self.insert_sourced_artifact(
            Sourced {
                provenance: Provenance::Synthesized,
                text: &new.text,
                title: new.title.as_ref(),
                category: new.category.as_ref(),
                tags: &new.tags,
                caveats: &new.caveats,
                cues: &new.cues,
            },
            sources,
        )
        .await
    }

    pub async fn synthesized_artifacts(&self, limit: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts
              WHERE provenance = 'synthesized' AND status = 'active' AND superseded_by IS NULL
              ORDER BY created_at DESC LIMIT ?",
            limit.max(0)
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    pub async fn count_synthesized_artifacts(&self) -> Result<i64> {
        use sqlx::Row;
        Ok(sqlx::query(
            "SELECT COUNT(*) AS n FROM artifacts
              WHERE provenance = 'synthesized' AND status = 'active' AND superseded_by IS NULL",
        )
        .fetch_one(&self.pool)
        .await?
        .get("n"))
    }

    pub async fn insert_artifacts(
        &self,
        corpus_id: &str,
        chunks: &[NewArtifact],
    ) -> Result<Vec<Chunk>> {
        self.insert_artifacts_with_provenance(corpus_id, chunks, Provenance::Captured)
            .await
    }

    /// Capture at `off`/`earned` writes passages through this.
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
                retired_at: None,
                reaped_at: None,
            };
            sqlx::query!(
                "INSERT INTO artifacts (id, corpus_id, provenance, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, activation, activated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, 1.0, ?)",
                &c.id,
                &c.corpus_id,
                provenance.as_str(),
                c.ordinal,
                &c.text,
                c.corpus_span.as_ref().and_then(|s| serde_json::to_string(s).ok()),
                &c.title,
                &c.category,
                serde_json::to_string(&c.tags).unwrap_or_else(|_| "[]".into()),
                c.embed_state.as_str(),
                c.created_at,
                c.segment_idx,
                serde_json::to_string(&c.caveats).unwrap_or_else(|_| "[]".into()),
                c.status.as_str(),
                c.last_verified_at,
                c.created_at
            )
                .execute(&mut *tx)
            .await?;
            out.push(c);
        }
        tx.commit().await?;
        Ok(out)
    }

    pub async fn get_artifact(&self, id: &str) -> Result<Chunk> {
        let row = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::NotFound)?;
        Ok(row_to_artifact(row))
    }

    /// The promotion path reads this to see what it supersedes and, on retry, what it wrote.
    pub async fn artifacts_for_segment(&self, corpus_id: &str, idx: i64) -> Result<Vec<Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE corpus_id = ? AND segment_idx = ? ORDER BY ordinal",
            corpus_id,
            idx
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// Promotion hands a new artifact its passages' access; all else uses `bump_activation`.
    pub async fn set_activation(&self, id: &str, value: f64, at: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET activation = ?, activated_at = ? WHERE id = ?",
            value,
            at,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A `hit` verdict naming it, on any recorded search.
    pub async fn artifact_confirmed(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM search_events WHERE verdict = 'hit' AND expect_id = ?",
            id
        )
        .fetch_one(&self.pool)
        .await?
            > 0)
    }

    /// Ids that name nothing are absent.
    pub async fn artifacts_by_ids(&self, ids: &[String]) -> Result<Vec<Chunk>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        // Deduplicated and chunked at 500: callers pass unbounded lists, and past
        // SQLITE_MAX_VARIABLE_NUMBER (999 before 3.32) the prepare fails.
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        let unique: Vec<&String> = ids.iter().filter(|id| seen.insert(*id)).collect();
        let mut out = Vec::with_capacity(unique.len());
        for chunk in unique.chunks(500) {
            let holes = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            // runtime: the IN list has one placeholder per id.
            let mut q = sqlx::query_as::<_, ArtifactRow>(sqlx::AssertSqlSafe(format!(
                "SELECT * FROM artifacts WHERE id IN ({holes})"
            )));
            for id in chunk {
                q = q.bind(*id);
            }
            out.extend(
                q.fetch_all(&self.pool)
                    .await?
                    .into_iter()
                    .map(row_to_artifact),
            );
        }
        Ok(out)
    }

    /// An id deleted since retrieval is absent from the map.
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

    /// Skips superseded rows that a promotion interleaved into the ordinals.
    pub async fn adjacent_artifacts(&self, corpus_id: &str, ordinal: i64) -> Result<Vec<Chunk>> {
        let before = sqlx::query_as!(
            ArtifactRow,
            "SELECT * FROM artifacts
             WHERE corpus_id = ? AND ordinal < ? AND status = 'active'
             ORDER BY ordinal DESC LIMIT 1",
            corpus_id,
            ordinal
        )
        .fetch_optional(&self.pool)
        .await?;
        let after = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts
             WHERE corpus_id = ? AND ordinal > ? AND status = 'active'
             ORDER BY ordinal ASC LIMIT 1",
            corpus_id,
            ordinal
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(before
            .into_iter()
            .chain(after)
            .map(row_to_artifact)
            .collect())
    }

    /// A merge has no `corpus_id` and so no reading order; a correlated subquery, because the
    /// rail redraws on every keystroke.
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
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE corpus_id = ? ORDER BY ordinal",
            corpus_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// The queue fragment polls ten sources every three seconds.
    pub async fn count_artifacts_for_corpus(&self, corpus_id: &str) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM artifacts WHERE corpus_id = ?",
            corpus_id
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Keyword fallback from SQLite for what the index cannot answer. `unembedded_only` limits
    /// it to rows with no vector yet. ASCII case-insensitive `LIKE`, newest first.
    pub async fn keyword_matches(
        &self,
        terms: &[String],
        unembedded_only: bool,
        limit: usize,
    ) -> Result<Vec<Chunk>> {
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "SELECT * FROM artifacts
              WHERE status = 'active' AND superseded_by IS NULL AND reaped_at IS NULL",
        );
        if unembedded_only {
            sql.push_str(" AND embed_state != 'embedded'");
        }
        for _ in terms {
            sql.push_str(
                " AND (text LIKE ? ESCAPE '\\' OR COALESCE(title, '') LIKE ? ESCAPE '\\')",
            );
        }
        sql.push_str(" ORDER BY created_at DESC, id LIMIT ?");
        // Only a fixed clause repeated per term is interpolated; every value is bound.
        // runtime: one LIKE clause per search term.
        let mut q = sqlx::query_as::<_, ArtifactRow>(sqlx::AssertSqlSafe(sql));
        for t in terms {
            let escaped = t
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            q = q.bind(pattern.clone()).bind(pattern);
        }
        let rows = q.bind(limit as i64).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// The embed job batches these into one call, so it needs rows, not a count.
    pub async fn pending_artifacts_for_corpus(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE corpus_id = ? AND embed_state = 'pending' ORDER BY ordinal",
            corpus_id
        ).fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// True means the source is on the per-chunk path; re-arming its batch would only fail again.
    pub async fn pending_artifacts_are_isolated(&self, corpus_id: &str) -> Result<bool> {
        let pending: Vec<String> = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM artifacts WHERE corpus_id = ? AND embed_state = 'pending'"#,
            corpus_id
        )
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

    /// The revision bump stops a mid-batch worker's `mark_embedded` from clearing this.
    /// `updated_at` is untouched: re-embedding changes no text.
    pub async fn reset_embed_state(&self, corpus_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts
             SET embed_state = 'pending', embed_model = NULL, embed_rev = embed_rev + 1
             WHERE corpus_id = ?",
            corpus_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Leaves `embed_state`: the embedder never sees a category.
    pub async fn update_artifact_category(&self, id: &str, category: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET category = ? WHERE id = ?",
                category,
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// An empty list is a clear.
    pub async fn update_artifact_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET tags = ? WHERE id = ?",
                serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()),
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// The title is part of the embedded text, so this invalidates the vector.
    pub async fn update_artifact_title(&self, id: &str, title: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts
                 SET title = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
                title,
                super::now(),
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn update_artifact_text(&self, id: &str, text: &str) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts
                 SET text = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
                text,
                super::now(),
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    /// The caller queues the embed, as with `update_artifact_text`.
    pub async fn absorb_passage(&self, id: &str, text: &str, span: &CorpusSpan) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts
                 SET text = ?, corpus_span = ?, embed_state = 'pending', embed_model = NULL,
                     embed_rev = embed_rev + 1, updated_at = ?
                 WHERE id = ?",
                text,
                serde_json::to_string(span).unwrap_or_default(),
                super::now(),
                id
            )
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

    /// `false` when a newer revision exists; the chunk stays pending. Whoever bumps the
    /// revision also enqueues, so a pending chunk always has a job coming.
    pub async fn mark_embedded(&self, id: &str, model: &str, rev: i64) -> Result<bool> {
        let res = sqlx::query!(
            "UPDATE artifacts SET embed_state = 'embedded', embed_model = ?
             WHERE id = ? AND embed_rev = ?",
            model,
            id,
            rev
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_embed_failed(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET embed_state = 'failed' WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_artifact(&self, id: &str) -> Result<()> {
        let res = sqlx::query!("DELETE FROM artifacts WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Window-less chunks are included so a re-split replaces rather than appends, except a
    /// `note`, which belongs to no window.
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

    /// Chunks inserted into the gap keep reading order without renumbering the source.
    pub async fn make_room_after(&self, corpus_id: &str, ordinal: i64, by: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET ordinal = ordinal + ? WHERE corpus_id = ? AND ordinal > ?",
            by,
            corpus_id,
            ordinal
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ordering by window, then by within-window number, reproduces reading order.
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
            sqlx::query!(
                "UPDATE artifacts SET ordinal = ? WHERE id = ?",
                n as i64,
                r.get::<String, _>("id")
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `status` moves in lockstep; this is the one place keeping the two columns consistent.
    pub async fn set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()> {
        self.set_superseded_by_with(artifact_id, by, None).await
    }

    /// One transaction: a hidden artifact with no open journal row can never be restored.
    pub async fn set_superseded_by_with(
        &self,
        artifact_id: &str,
        by: Option<&str>,
        journal: Option<&crate::store::actions::NewAction>,
    ) -> Result<()> {
        let status = if by.is_some() {
            ArtifactStatus::Superseded
        } else {
            ArtifactStatus::Active
        };
        // The marker and `retired_at` ride the statement they describe. COALESCE keeps the first
        // retirement's clock; it clears on reactivation.
        let res = sqlx::query(
            "UPDATE artifacts SET superseded_by = ?, status = ?, lifecycle_dirty = 1,
                    retired_at = CASE WHEN ? THEN COALESCE(retired_at, ?) ELSE NULL END
             WHERE id = ?",
        )
        .bind(by)
        .bind(status.as_str())
        .bind(by.is_some())
        .bind(now())
        .bind(artifact_id);
        let mut tx = self.pool.begin().await?;
        let res = res.execute(&mut *tx).await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        if let Some(a) = journal {
            crate::store::actions::insert(&mut *tx, a).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Does not touch `superseded_by`; use `set_superseded_by(id, None)` to clear one.
    pub async fn set_artifact_status(&self, id: &str, status: ArtifactStatus) -> Result<()> {
        self.set_artifact_status_with(id, status, None).await
    }

    pub async fn set_artifact_status_with(
        &self,
        id: &str,
        status: ArtifactStatus,
        journal: Option<&crate::store::actions::NewAction>,
    ) -> Result<()> {
        // Marks dirty and stamps `retired_at` in the statement that moves the status, as
        // `set_superseded_by` does.
        let mut tx = self.pool.begin().await?;
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET status = ?, lifecycle_dirty = 1,
                        retired_at = CASE WHEN ? THEN COALESCE(retired_at, ?) ELSE NULL END
                 WHERE id = ?",
                status.as_str(),
                status != ArtifactStatus::Active,
                now(),
                id
            )
            .execute(&mut *tx)
            .await?,
        )?;
        if let Some(a) = journal {
            crate::store::actions::insert(&mut *tx, a).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn trash_artifact(&self, id: &str, at: i64) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET trashed_from = status, status = 'trashed', trashed_at = ?,
                        lifecycle_dirty = 1
                 WHERE id = ? AND status <> 'trashed'",
                at,
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn trashed_from(&self, id: &str) -> Result<Option<ArtifactStatus>> {
        let row: Option<Option<String>> = sqlx::query_scalar!(
            "SELECT trashed_from FROM artifacts WHERE id = ? AND status = 'trashed'",
            id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|s| ArtifactStatus::parse(s.as_deref().unwrap_or("active"))))
    }

    pub async fn untrash_artifact(&self, id: &str) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET status = COALESCE(trashed_from, 'active'),
                        trashed_from = NULL, trashed_at = NULL, lifecycle_dirty = 1
                 WHERE id = ? AND status = 'trashed'",
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn trash(&self) -> Result<Vec<(Chunk, i64)>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE status = 'trashed' ORDER BY trashed_at DESC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let at = r.trashed_at.unwrap_or(0);
                (row_to_artifact(r), at)
            })
            .collect())
    }

    /// Ids in Trash since before `cutoff`.
    pub async fn trashed_before(&self, cutoff: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM artifacts WHERE status = 'trashed' AND trashed_at < ?"#,
            cutoff
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Only writes that did not finish: hidden artifacts only ever grow, so scanning them all
    /// would be truncated forever.
    pub async fn dirty_lifecycle_artifacts(&self, limit: usize) -> Result<Vec<Chunk>> {
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE lifecycle_dirty = 1 ORDER BY id LIMIT ?",
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// Needed for the reveal direction, which writes the payload first; a crash between the
    /// stores would otherwise leave unannounced drift.
    pub async fn mark_lifecycle_dirty(&self, id: &str) -> Result<()> {
        sqlx::query!("UPDATE artifacts SET lifecycle_dirty = 1 WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Never before the payload write is acknowledged, or a failure becomes permanent drift.
    pub async fn clear_lifecycle_dirty(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            sqlx::query!("UPDATE artifacts SET lifecycle_dirty = 0 WHERE id = ?", id)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn set_last_verified_at(&self, id: &str, at: i64) -> Result<()> {
        self.expect_updated(
            sqlx::query!(
                "UPDATE artifacts SET last_verified_at = ? WHERE id = ?",
                at,
                id
            )
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn list_all_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(ALL_ARTIFACT_IDS).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Pending and `failed` rows are not drift. Reaped stubs are excluded: `bury` keeps
    /// `embed_state`, and re-embedding a stub would undo the burial every pass.
    pub async fn list_embedded_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(EMBEDDED_ARTIFACT_IDS)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Passages and merges are never anchors. Walked behind a wrapping cursor: the "never
    /// asked" test lives in the other database, so the `LIMIT` applies after it.
    pub async fn list_unrelated_artifact_ids(&self, limit: usize) -> Result<Vec<String>> {
        const CURSOR: &str = "repair.relate_backstop_after";
        // An unknown share of any window is already armed.
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
                AND a.provenance <> 'passage' AND a.provenance <> 'merged'
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
        // A short window ends the walk; the next lap starts from the oldest again.
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

    /// No foreign key on `superseded_by`, so a deleted keeper leaves its loser hidden. A read:
    /// the caller clears the payload first. Reaped stubs are excluded; `exhume` undoes a burial.
    pub async fn dangling_superseded(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM artifacts
              WHERE superseded_by IS NOT NULL
                AND reaped_at IS NULL
                AND status <> 'trashed'
                AND superseded_by NOT IN (SELECT id FROM artifacts WHERE status <> 'trashed')",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// A superseded winner's losers must be re-pointed, or they lead to a dead end.
    pub async fn artifacts_superseded_by(&self, winner_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts WHERE superseded_by = ?")
            .bind(winner_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    pub async fn superseded_artifacts(&self, limit: i64) -> Result<Vec<Chunk>> {
        // Not a buried row: the graveyard lists those.
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts WHERE superseded_by IS NOT NULL AND reaped_at IS NULL
              ORDER BY created_at DESC LIMIT ?",
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    pub async fn artifacts_by_status(
        &self,
        status: ArtifactStatus,
        limit: i64,
    ) -> Result<Vec<Chunk>> {
        // Neither buried nor superseded rows: each is listed elsewhere, and twice confused clients.
        let rows = sqlx::query_as!(
            crate::store::artifacts::ArtifactRow,
            "SELECT * FROM artifacts
              WHERE status = ? AND reaped_at IS NULL AND superseded_by IS NULL
              ORDER BY created_at DESC LIMIT ?",
            status.as_str(),
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// An empty list clears, so a re-checked chunk sheds a stale warning.
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
        sqlx::query!(
            "UPDATE artifacts SET flags = ?, flag_detail = ? WHERE id = ?",
            json,
            detail,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn clear_artifact_flags(&self, id: &str) -> Result<()> {
        self.set_artifact_flags(id, &[], None).await
    }

    /// Two columns, not the full row: `arm_dedupe` checks 200 pairs per tick. `None` means gone.
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

    /// Without lowering `source_count`, the orphan flag returns on the next sweep.
    pub async fn accept_source_loss(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts
                SET source_count = (SELECT COUNT(*) FROM artifact_sources WHERE child_id = ?)
              WHERE id = ? AND provenance = 'merged'",
            id,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Shared verbatim by `reap_candidates` and `bury`'s re-check, so they cannot drift; binds
    /// three copies of `now - min_age_secs`. Aliased `art` because the reminder clause joins
    /// `artifacts` again, and a merged candidate's corpora come from `artifact_sources`.
    const REAPABLE: &str = "(art.status != 'active' OR art.superseded_by IS NOT NULL)
                AND art.status != 'trashed'
                AND art.reaped_at IS NULL
                AND art.retired_at IS NOT NULL AND art.retired_at < ?
                AND NOT EXISTS (SELECT 1 FROM moments m
                                 JOIN artifacts live ON live.id = m.artifact_id
                                WHERE live.status = 'active'
                                  AND m.kind = 'due' AND m.done_at IS NULL
                                  AND COALESCE(m.origin_corpus_id, live.corpus_id) IN (
                                        SELECT art.corpus_id
                                         UNION ALL
                                        SELECT r.corpus_id FROM artifact_sources s
                                          JOIN artifacts r ON r.id = s.root_id
                                         WHERE s.child_id = art.id))
                AND art.id NOT IN (
                  SELECT s.root_id FROM artifact_sources s
                    JOIN artifacts m ON m.id = s.child_id
                   WHERE m.provenance = 'merged'
                     AND m.status = 'active' AND m.superseded_by IS NULL
                     AND m.created_at > ?
                  UNION
                  SELECT s.via_id FROM artifact_sources s
                    JOIN artifacts m ON m.id = s.child_id
                   WHERE m.provenance = 'merged'
                     AND m.status = 'active' AND m.superseded_by IS NULL
                     AND m.created_at > ?
                     AND s.via_id IS NOT NULL
                )";

    /// Open reminders on the note's active artifacts hold a row back (`kind = 'due'` only).
    /// Age reads `retired_at` alone. Rules SQLite can state are here, before the `LIMIT`;
    /// "seen since retired" lives on the vector point (see `jobs::reap::nominees`).
    pub async fn reap_candidates(&self, min_age_secs: i64, limit: i64) -> Result<Vec<Chunk>> {
        // runtime: shares the REAPABLE predicate with the graveyard insert.
        let rows = sqlx::query_as::<_, ArtifactRow>(sqlx::AssertSqlSafe(format!(
            "SELECT * FROM artifacts art
              WHERE {}
              ORDER BY art.retired_at ASC
              LIMIT ?",
            Self::REAPABLE
        )))
        .bind(now() - min_age_secs)
        .bind(now() - min_age_secs)
        .bind(now() - min_age_secs)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_artifact).collect())
    }

    /// A fresh stamp is merely slow; an invented historical one would reap too early.
    pub async fn stamp_unaged_retired(&self) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE artifacts SET retired_at = ?
              WHERE (status != 'active' OR superseded_by IS NOT NULL) AND status != 'trashed'
                AND retired_at IS NULL",
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Only a rescue restarts the clock; otherwise the rescued row is judged every run.
    pub async fn restamp_retired(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ? AND retired_at IS NOT NULL",
            now(),
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Copy first, then wipe, in one transaction; the FTS trigger empties the entry. Re-checks
    /// `REAPABLE` with the caller's `min_age_secs`, since minutes pass during the model call.
    pub async fn bury(
        &self,
        id: &str,
        meta_json: &str,
        min_age_secs: i64,
        vec: Option<&[f32]>,
        embed_model: Option<&str>,
        journal: &crate::store::actions::NewAction,
    ) -> Result<()> {
        let reaped_at = now();
        let cutoff = reaped_at - min_age_secs;
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO graveyard (id, title, text, meta_json, reaped_at, vec, embed_model)
             SELECT art.id, art.title, art.text, ?, ?, ?, ? FROM artifacts art
              WHERE art.id = ? AND {}",
            Self::REAPABLE
        )))
        .bind(meta_json)
        .bind(reaped_at)
        .bind(vec.map(crate::store::feedback::vec_to_blob))
        .bind(embed_model)
        .bind(id)
        .bind(cutoff)
        .bind(cutoff)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        // `lifecycle_dirty` rides the wipe's transaction, or a crash leaves the text in Qdrant.
        sqlx::query!(
            "UPDATE artifacts SET text = '', reaped_at = ?, lifecycle_dirty = 1 WHERE id = ?",
            reaped_at,
            id
        )
        .execute(&mut *tx)
        .await?;
        crate::store::actions::insert(&mut *tx, journal).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Only graves whose stub exists: this list is the Restore button.
    pub async fn graveyard_list(&self, limit: i64) -> Result<Vec<Grave>> {
        sqlx::query(
            "SELECT g.id AS id, g.title AS title, g.meta_json AS meta_json,
                    g.reaped_at AS reaped_at
               FROM graveyard g
               JOIN artifacts a ON a.id = g.id
              ORDER BY g.reaped_at DESC, g.id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            let meta: serde_json::Value =
                serde_json::from_str(&r.get::<String, _>("meta_json")).unwrap_or_default();
            Ok(Grave {
                id: r.get("id"),
                title: r.get("title"),
                reaped_at: r.get("reaped_at"),
                reason: meta
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
    }

    /// Compared in Rust: the graveyard is small. Only graves whose stub exists, or
    /// `retract::rule_two` picks an orphan that cannot be restored.
    pub async fn graveyard_vectors(&self, embed_model: &str) -> Result<Vec<(String, Vec<f32>)>> {
        Ok(sqlx::query(
            "SELECT g.id AS id, g.vec AS vec FROM graveyard g
                   JOIN artifacts a ON a.id = g.id
                  WHERE g.vec IS NOT NULL AND g.embed_model = ?",
        )
        .bind(embed_model)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("id"),
                crate::store::feedback::blob_to_vec(&r.get::<Vec<u8>, _>("vec")),
            )
        })
        .collect())
    }

    /// `bury`'s guard for the other verdict: the snapshot is minutes old by now.
    pub async fn supersede_if_reapable(&self, id: &str, by: &str, min_age_secs: i64) -> Result<()> {
        let cutoff = now() - min_age_secs;
        let res = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE artifacts SET superseded_by = ?, status = ?, lifecycle_dirty = 1,
                    retired_at = COALESCE(retired_at, ?)
              WHERE id = ? AND id IN (SELECT art.id FROM artifacts art
                                       WHERE art.id = ? AND {})",
            Self::REAPABLE
        )))
        .bind(by)
        .bind(ArtifactStatus::Superseded.as_str())
        .bind(now())
        .bind(id)
        .bind(id)
        .bind(cutoff)
        .bind(cutoff)
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// One transaction; the grave row goes only where the artifact was restored.
    /// `lifecycle_dirty` is cleared: there is no point to write a payload onto until the embed.
    pub async fn exhume(&self, id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let Some(row) = sqlx::query("SELECT text FROM graveyard WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(false);
        };
        let text: String = row.get("text");
        let res = sqlx::query!(
            "UPDATE artifacts
                SET text = ?, reaped_at = NULL, embed_state = 'pending',
                    embed_model = NULL, embed_rev = embed_rev + 1,
                    lifecycle_dirty = 0, updated_at = ?
              WHERE id = ? AND reaped_at IS NOT NULL",
            &text,
            super::now(),
            id
        )
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query!("DELETE FROM graveyard WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn graveyard_row(&self, id: &str) -> Result<Option<(String, String, i64)>> {
        let row = sqlx::query("SELECT text, meta_json, reaped_at FROM graveyard WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| (r.get("text"), r.get("meta_json"), r.get("reaped_at"))))
    }

    /// How many retired rows still hold their text — the standing count the
    /// status line shows beside what the last sweep did.
    pub async fn retired_unreaped_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM artifacts
              WHERE (status != 'active' OR superseded_by IS NOT NULL) AND status != 'trashed'
                AND reaped_at IS NULL"
        )
        .fetch_one(&self.pool)
        .await?)
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

    fn reap_row(id: &str) -> crate::store::actions::NewAction {
        crate::store::actions::NewAction {
            job: crate::store::actions::Job::Reap,
            kind: crate::store::actions::Kind::Reap,
            subject_id: id.to_string(),
            survivor_id: None,
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: None,
        }
    }
    use crate::store::Store;

    fn nc(ord: i64, text: &str) -> NewArtifact {
        NewArtifact {
            ordinal: ord,
            text: text.to_string(),
            corpus_span: Some(CorpusSpan {
                start_line: 1,
                end_line: 4,
                source: crate::store::artifacts::SpanSource::Located,
            }),
            title: Some(format!("title {ord}")),
            category: Some("procedure".into()),
            tags: vec!["forensics".into(), "windows".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_captured_artifact_is_captured_and_names_its_corpus() {
        // `provenance` is the discriminator every consumer branches on, never `corpus_id IS NULL`.
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
        let s = Store::memory().await.unwrap();
        let root = a_passage(&s).await;

        let refused = s
            .insert_merged_artifact(
                &NewMerged {
                    title: Some("merged".into()),
                    text: "rewritten".into(),
                    ..Default::default()
                },
                &[root],
            )
            .await;

        assert!(refused.is_err(), "a passage is not a merge root");
    }

    #[tokio::test]
    async fn a_synthesized_artifact_may_still_name_passage_sources() {
        // A synthesis draws on passages by design, so a table constraint would break it.
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
    async fn a_windowless_chunk_is_swept_with_the_window_and_a_note_is_not() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let mut owned = nc(0, "written for window one");
        owned.segment_idx = Some(1);
        let mut elsewhere = nc(1, "written for window two");
        elsewhere.segment_idx = Some(2);
        let orphan = nc(2, "written before windows existed");
        s.insert_artifacts(&src.id, &[owned, elsewhere, orphan])
            .await
            .unwrap();
        s.insert_artifacts_with_provenance(
            &src.id,
            &[nc(3, "what the person said about the file")],
            Provenance::Note,
        )
        .await
        .unwrap();

        let swept = s.artifact_ids_for_segment(&src.id, 1).await.unwrap();
        let mut got = Vec::new();
        for id in &swept {
            got.push(s.get_artifact(id).await.unwrap().text);
        }
        got.sort();
        assert_eq!(
            got,
            vec![
                "written before windows existed".to_string(),
                "written for window one".to_string(),
            ],
            "the note or another window's chunk was swept"
        );
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
        // Re-embedding changes no text, so `updated_at` must not move.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let c = s.insert_artifacts(&src.id, &[nc(0, "x")]).await.unwrap()[0].clone();

        s.reset_embed_state(&src.id).await.unwrap();

        let stamp: i64 =
            sqlx::query_scalar!("SELECT updated_at FROM artifacts WHERE id = ?", &c.id)
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
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let c = s
            .insert_artifacts(&src.id, &[nc(0, "before")])
            .await
            .unwrap()
            .remove(0);

        let before: i64 =
            sqlx::query_scalar!("SELECT updated_at FROM artifacts WHERE id = ?", &c.id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(before, 0, "a fresh row has never been edited");

        s.update_artifact_text(&c.id, "after").await.unwrap();

        let after: i64 =
            sqlx::query_scalar!("SELECT updated_at FROM artifacts WHERE id = ?", &c.id)
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
        // Listing must not clear: a failed vector write would leave the artifact hidden and lost.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "loser".into(),
                    ..Default::default()
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
        // A cascaded-away pair is not a store fault.
        assert_eq!(s.artifact_in_results("no-such-id").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_fts_index_is_gone() {
        // The lexical half lives in Qdrant; the SQLite index and triggers must be gone.
        let s = Store::memory().await.unwrap();
        let leftovers: Vec<String> = sqlx::query_scalar!(
            r#"SELECT name AS "name!" FROM sqlite_master
             WHERE name = 'artifacts_fts' OR name LIKE 'artifacts_a%'"#
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
    async fn a_long_repetitive_list_of_ids_is_still_one_answer() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        // Well past 999, and mostly repeats.
        let mut ids: Vec<String> = (0..2_000).map(|i| format!("gone-{i}")).collect();
        for _ in 0..40 {
            ids.push(made[0].id.clone());
            ids.push(made[1].id.clone());
        }
        let got = s.artifacts_by_ids(&ids).await.unwrap();
        let mut texts: Vec<&str> = got.iter().map(|c| c.text.as_str()).collect();
        texts.sort_unstable();
        assert_eq!(texts, vec!["a", "b"], "a repeated id came back twice");
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

    #[tokio::test]
    async fn the_relate_backstop_never_lists_a_merge_either() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "a"), nc(1, "b")])
            .await
            .unwrap();
        let m = s
            .insert_merged_artifact(
                &NewMerged {
                    text: "a and b".into(),
                    title: Some("both".into()),
                    ..Default::default()
                },
                &[made[0].id.clone(), made[1].id.clone()],
            )
            .await
            .unwrap();
        for id in [&made[0].id, &made[1].id, &m.id] {
            s.mark_embedded(id, "fake", 0).await.unwrap();
        }

        let ids = s.list_unrelated_artifact_ids(10).await.unwrap();

        assert!(
            !ids.contains(&m.id),
            "the repair pass would arm a relate unit for a merge"
        );
        assert_eq!(ids.len(), 2, "it should still list the two captured rows");
    }

    #[tokio::test]
    async fn the_relate_backstop_walks_past_a_window_that_is_already_armed() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made: Vec<_> = (0..12).map(|i| nc(i, "text")).collect();
        for a in s.insert_artifacts(&src.id, &made).await.unwrap() {
            s.mark_embedded(&a.id, "fake", 0).await.unwrap();
        }
        let ordered: Vec<String> =
            sqlx::query_scalar!(r#"SELECT id AS "id!" FROM artifacts ORDER BY created_at, id"#)
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

        sqlx::query!("DELETE FROM jobs WHERE target_id = ?", &ids[0])
            .execute(&s.control.pool)
            .await
            .unwrap();

        assert_eq!(
            s.list_unrelated_artifact_ids(8).await.unwrap(),
            vec![ids[0].clone()],
            "the walk did not start over"
        );
    }

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
                    ..Default::default()
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
        let (value, stamp, _) = act[&made[0].id];
        assert_eq!((value, stamp), (7.25, 99));
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
                    fold_onto: None,
                    query: "q".into(),
                    door: crate::store::feedback::Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        s.judge_hit(&ev, &made[0].id, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn retirement_stamps_retired_at_and_reactivation_clears_it() {
        // The age rule reads this stamp; it must be written on retire and cleared on reactivate.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "one"), nc(1, "two")])
            .await
            .unwrap();

        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .retired_at
                .is_some()
        );
        s.set_artifact_status(&made[0].id, ArtifactStatus::Active)
            .await
            .unwrap();
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .retired_at
                .is_none()
        );

        s.set_superseded_by(&made[0].id, Some(&made[1].id))
            .await
            .unwrap();
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .retired_at
                .is_some()
        );
        s.set_superseded_by(&made[0].id, None).await.unwrap();
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .retired_at
                .is_none()
        );
    }

    async fn backdate_retired_at(s: &Store, id: &str, secs_ago: i64) {
        sqlx::query!(
            "UPDATE artifacts SET retired_at = ? WHERE id = ?",
            now() - secs_ago,
            id
        )
        .execute(&s.pool)
        .await
        .unwrap();
    }

    async fn insert_open_moment(s: &Store, artifact_id: &str) {
        sqlx::query!(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, source, created_at)
             VALUES (?, ?, 'due', ?, 'UTC', 'set', ?)",
            new_id(),
            artifact_id,
            now() + 3_600,
            now()
        )
        .execute(&s.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reap_candidates_apply_age_status_and_moment_rules() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[nc(0, "old"), nc(1, "young"), nc(2, "unreachable-reminder")],
            )
            .await
            .unwrap();
        for c in &made {
            s.set_artifact_status(&c.id, ArtifactStatus::Deprecated)
                .await
                .unwrap();
        }
        backdate_retired_at(&s, &made[0].id, 100 * 86_400).await;
        // made[1] keeps its fresh stamp: too young.
        backdate_retired_at(&s, &made[2].id, 100 * 86_400).await;
        insert_open_moment(&s, &made[2].id).await;

        // A second note whose live artifact still carries an open reminder; distinct text, since
        // `insert_corpus` deduplicates on content.
        let other = s.insert_corpus("another note", "web", None).await.unwrap();
        let pair = s
            .insert_artifacts(
                &other.id,
                &[nc(0, "live"), nc(1, "retired-beside-a-live-reminder")],
            )
            .await
            .unwrap();
        insert_open_moment(&s, &pair[0].id).await;
        s.set_artifact_status(&pair[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &pair[1].id, 100 * 86_400).await;

        let got = s.reap_candidates(90 * 86_400, 20).await.unwrap();
        let mut ids = got.iter().map(|c| c.id.as_str()).collect::<Vec<_>>();
        ids.sort_unstable();
        let mut want = vec![made[0].id.as_str(), made[2].id.as_str()];
        want.sort_unstable();
        assert_eq!(
            ids, want,
            "the old retirements, the note with a live reminder aside"
        );
    }

    #[tokio::test]
    async fn a_reminder_carried_onto_a_merged_artifact_still_holds_its_note_back() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("the note behind the reminder", "web", None)
            .await
            .unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "live"), nc(1, "retired")])
            .await
            .unwrap();
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &made[1].id, 100 * 86_400).await;

        let merged = s
            .insert_merged_artifact(
                &NewMerged {
                    text: "the merged text".into(),
                    title: Some("merged".into()),
                    ..Default::default()
                },
                std::slice::from_ref(&made[0].id),
            )
            .await
            .unwrap()
            .id;
        assert!(
            s.get_artifact(&merged).await.unwrap().corpus_id.is_none(),
            "a merged artifact belongs to no one note, which is the whole point"
        );

        sqlx::query!(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, source, origin_corpus_id, created_at)
             VALUES (?, ?, 'due', ?, 'UTC', 'set', ?, ?)",
            new_id(),
            &merged,
            now() + 3_600,
            &src.id,
            now()
        )
            .execute(&s.pool)
        .await
        .unwrap();

        assert!(
            s.has_open_reminder_for_corpus(&src.id).await.unwrap(),
            "the note is one somebody is still being reminded about"
        );
        assert!(
            s.reap_candidates(90 * 86_400, 20).await.unwrap().is_empty(),
            "so nothing of it is nominated"
        );
        assert!(
            matches!(
                s.bury(
                    &made[1].id,
                    "{}",
                    90 * 86_400,
                    None,
                    None,
                    &reap_row(&made[1].id)
                )
                .await,
                Err(Error::NotFound)
            ),
            "and the burial re-check refuses on the same grounds"
        );
        assert_eq!(s.get_artifact(&made[1].id).await.unwrap().text, "retired");
    }

    #[tokio::test]
    async fn a_superseded_merge_is_held_back_by_the_reminder_on_the_note_it_was_made_from() {
        let s = Store::memory().await.unwrap();
        let src = s
            .insert_corpus("the note behind the reminder", "web", None)
            .await
            .unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "folded in"), nc(1, "still standing")])
            .await
            .unwrap();
        insert_open_moment(&s, &made[1].id).await;

        let merged = s
            .insert_merged_artifact(
                &NewMerged {
                    text: "the merged text".into(),
                    title: Some("merged".into()),
                    ..Default::default()
                },
                std::slice::from_ref(&made[0].id),
            )
            .await
            .unwrap()
            .id;
        assert!(
            s.get_artifact(&merged).await.unwrap().corpus_id.is_none(),
            "which is the NULL the guard used to compare against"
        );
        s.set_artifact_status(&merged, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &merged, 100 * 86_400).await;
        backdate_retired_at(&s, &made[0].id, 100 * 86_400).await;

        assert!(
            s.reap_candidates(90 * 86_400, 20).await.unwrap().is_empty(),
            "the merge carries the note's text, and the note is one somebody \
             is still being reminded about"
        );
        assert!(
            matches!(
                s.bury(&merged, "{}", 90 * 86_400, None, None, &reap_row(&merged))
                    .await,
                Err(Error::NotFound)
            ),
            "and the burial re-check refuses on the same grounds"
        );
        assert_eq!(
            s.get_artifact(&merged).await.unwrap().text,
            "the merged text"
        );
    }

    #[tokio::test]
    async fn stamping_gives_unaged_retired_rows_a_clock_and_nothing_else() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "retired"), nc(1, "active")])
            .await
            .unwrap();
        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE artifacts SET retired_at = NULL WHERE id = ?",
            &made[0].id
        )
        .execute(&s.pool)
        .await
        .unwrap();
        assert_eq!(s.stamp_unaged_retired().await.unwrap(), 1);
        assert!(
            s.get_artifact(&made[0].id)
                .await
                .unwrap()
                .retired_at
                .is_some()
        );
        assert!(
            s.get_artifact(&made[1].id)
                .await
                .unwrap()
                .retired_at
                .is_none()
        );
        assert_eq!(
            s.stamp_unaged_retired().await.unwrap(),
            0,
            "stamping is once"
        );
    }

    #[tokio::test]
    async fn a_reminder_set_while_the_judgement_was_in_flight_stops_the_burial() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "live"), nc(1, "retired")])
            .await
            .unwrap();
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &made[1].id, 100 * 86_400).await;
        assert_eq!(s.reap_candidates(90 * 86_400, 20).await.unwrap().len(), 1);

        insert_open_moment(&s, &made[0].id).await;
        assert!(
            s.reap_candidates(90 * 86_400, 20).await.unwrap().is_empty(),
            "the nomination would no longer be made"
        );
        assert!(
            matches!(
                s.bury(
                    &made[1].id,
                    "{}",
                    90 * 86_400,
                    None,
                    None,
                    &reap_row(&made[1].id)
                )
                .await,
                Err(Error::NotFound)
            ),
            "so the verdict about it must not be applied either"
        );
        assert_eq!(s.get_artifact(&made[1].id).await.unwrap().text, "retired");
    }

    #[tokio::test]
    async fn a_reaped_row_is_not_drift_for_the_heal_to_re_embed() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "ephemeral")])
            .await
            .unwrap();
        let id = made[0].id.clone();
        let rev = s.get_artifact(&id).await.unwrap().embed_rev;
        assert!(s.mark_embedded(&id, "m", rev).await.unwrap());
        assert_eq!(
            s.list_embedded_artifact_ids().await.unwrap(),
            vec![id.clone()]
        );

        s.set_artifact_status(&id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &id, 100 * 86_400).await;
        s.bury(&id, "{}", 90 * 86_400, None, None, &reap_row(&id))
            .await
            .unwrap();
        assert!(
            s.list_embedded_artifact_ids().await.unwrap().is_empty(),
            "a burial is not a write that went missing"
        );

        assert!(s.exhume(&id).await.unwrap());
        assert_eq!(
            s.get_artifact(&id).await.unwrap().embed_state,
            EmbedState::Pending,
            "and the restore is what asks for a point again"
        );
    }

    #[tokio::test]
    async fn a_deprecated_artifact_that_is_also_superseded_is_listed_once() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "clinic hours"), nc(1, "clinic services")])
            .await
            .unwrap();
        let (loser, winner) = (&made[0].id, &made[1].id);
        s.set_artifact_status(loser, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        s.set_superseded_by(loser, Some(winner)).await.unwrap();

        let deprecated = s
            .artifacts_by_status(ArtifactStatus::Deprecated, 10)
            .await
            .unwrap();
        assert!(
            deprecated.is_empty(),
            "the superseded list already has it, and names the winner besides"
        );
        let superseded = s.superseded_artifacts(10).await.unwrap();
        assert_eq!(superseded.len(), 1);
        assert_eq!(&superseded[0].id, loser);
    }

    #[tokio::test]
    async fn burying_an_artifact_that_is_live_again_does_nothing() {
        // The verdict can land on an artifact somebody has since put back.
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "still wanted")])
            .await
            .unwrap();
        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        s.set_artifact_status(&made[0].id, ArtifactStatus::Active)
            .await
            .unwrap();

        assert!(matches!(
            s.bury(&made[0].id, "{}", 0, None, None, &reap_row(&made[0].id))
                .await,
            Err(Error::NotFound)
        ));
        let row = s.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(
            row.text, "still wanted",
            "the text of a live row is untouched"
        );
        assert!(row.reaped_at.is_none());
        assert!(s.graveyard_row(&made[0].id).await.unwrap().is_none());

        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &made[0].id, 60).await;
        s.bury(&made[0].id, "{}", 0, None, None, &reap_row(&made[0].id))
            .await
            .unwrap();
        assert!(matches!(
            s.bury(&made[0].id, "{}", 0, None, None, &reap_row(&made[0].id))
                .await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn a_buried_stub_is_not_resurrected_by_the_dangling_heal() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "the loser"), nc(1, "the keeper")])
            .await
            .unwrap();
        let (loser, keeper) = (made[0].id.clone(), made[1].id.clone());
        s.set_superseded_by(&loser, Some(&keeper)).await.unwrap();
        backdate_retired_at(&s, &loser, 60).await;
        s.bury(&loser, "{}", 0, None, None, &reap_row(&loser))
            .await
            .unwrap();

        s.delete_artifact(&keeper).await.unwrap();
        assert!(
            s.dangling_superseded().await.unwrap().is_empty(),
            "there is no surviving text to reveal — it is in the graveyard"
        );
        let row = s.get_artifact(&loser).await.unwrap();
        assert!(row.reaped_at.is_some());
        assert_ne!(row.status, ArtifactStatus::Active);
    }

    #[tokio::test]
    async fn exhuming_restores_the_text_and_asks_for_a_fresh_embedding() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "ephemeral fact xylophone")])
            .await
            .unwrap();
        let id = made[0].id.clone();
        s.set_artifact_status(&id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        let before = s.get_artifact(&id).await.unwrap().embed_rev;
        backdate_retired_at(&s, &id, 60).await;
        s.bury(&id, "{}", 0, None, None, &reap_row(&id))
            .await
            .unwrap();

        assert!(s.exhume(&id).await.unwrap());
        let row = s.get_artifact(&id).await.unwrap();
        assert_eq!(row.text, "ephemeral fact xylophone");
        assert!(row.reaped_at.is_none());
        assert!(row.embed_rev > before, "the point was deleted at burial");
        assert!(
            s.graveyard_row(&id).await.unwrap().is_none(),
            "the grave is emptied only where the row was actually restored"
        );
        assert!(
            !s.exhume(&id).await.unwrap(),
            "a restore that has already happened is not an error"
        );
    }

    #[tokio::test]
    async fn bury_copies_then_wipes_in_one_transaction() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("raw", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(&src.id, &[nc(0, "ephemeral fact xylophone"), nc(1, "two")])
            .await
            .unwrap();
        s.set_artifact_status(&made[0].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        backdate_retired_at(&s, &made[0].id, 60).await;
        s.bury(
            &made[0].id,
            r#"{"reason":"nothing new"}"#,
            0,
            None,
            None,
            &reap_row(&made[0].id),
        )
        .await
        .unwrap();

        let row = s.get_artifact(&made[0].id).await.unwrap();
        assert_eq!(row.text, "");
        assert!(row.reaped_at.is_some());
        assert!(
            row.status != ArtifactStatus::Active,
            "the stub keeps its status"
        );
        let (text, meta, _) = s.graveyard_row(&made[0].id).await.unwrap().unwrap();
        assert!(
            text.contains("xylophone"),
            "the graveyard holds the full text"
        );
        assert!(meta.contains("nothing new"));
        assert!(
            s.dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .iter()
                .any(|c| c.id == made[0].id),
            "the wipe and the drift marker commit together, so a crash between \
             them cannot leave a stub whose vector still holds the text"
        );

        assert_eq!(s.retired_unreaped_count().await.unwrap(), 0);
        s.set_artifact_status(&made[1].id, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert_eq!(s.retired_unreaped_count().await.unwrap(), 1);
    }
}
