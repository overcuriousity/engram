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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CorpusSpan {
    pub start_line: i64,
    pub end_line: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Chunk {
    pub id: String,
    pub corpus_id: String,
    pub ordinal: i64,
    pub text: String,
    pub corpus_span: Option<CorpusSpan>,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub embed_state: EmbedState,
    pub embed_model: Option<String>,
    pub created_at: i64,
    #[serde(skip)]
    pub embed_rev: i64,
    pub segment_idx: Option<i64>,
    pub flags: Vec<String>,
    pub flag_detail: Option<String>,
    pub superseded_by: Option<String>,
    pub caveats: Vec<String>,
    pub status: ArtifactStatus,
    pub last_verified_at: Option<i64>,
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

#[derive(Debug, Clone)]
pub struct RestoredArtifact {
    pub id: String,
    pub corpus_id: String,
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub status: ArtifactStatus,
    pub last_verified_at: Option<i64>,
    pub superseded_by: Option<String>,
}

fn row_to_artifact(r: &sqlx::sqlite::SqliteRow) -> Chunk {
    let tags_json: String = r.get("tags");
    let span_json: Option<String> = r.get("corpus_span");
    let flags_json: Option<String> = r.get("flags");
    Chunk {
        id: r.get("id"),
        corpus_id: r.get("corpus_id"),
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
                corpus_id: corpus_id.to_string(),
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
                "INSERT INTO artifacts (id, corpus_id, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?)",
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

    pub async fn restore_artifact(&self, c: &RestoredArtifact) -> Result<bool> {
        let res = sqlx::query(
            "INSERT INTO artifacts (id, corpus_id, ordinal, text, corpus_span, title, category, tags, embed_state, embed_model, created_at, segment_idx, caveats, status, last_verified_at, superseded_by)
             VALUES (?, ?, 0, ?, NULL, ?, ?, ?, 'pending', NULL, ?, NULL, '[]', ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(&c.id)
        .bind(&c.corpus_id)
        .bind(&c.text)
        .bind(&c.title)
        .bind(&c.category)
        .bind(serde_json::to_string(&c.tags).unwrap_or_else(|_| "[]".into()))
        .bind(c.created_at)
        .bind(c.status.as_str())
        .bind(c.last_verified_at)
        .bind(&c.superseded_by)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn get_artifact(&self, id: &str) -> Result<Chunk> {
        let row = sqlx::query("SELECT * FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row_to_artifact(&row))
    }

    pub async fn artifacts_for_corpus(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query("SELECT * FROM artifacts WHERE corpus_id = ? ORDER BY ordinal")
            .bind(corpus_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    pub async fn count_artifacts_for_corpus(&self, corpus_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE corpus_id = ?")
                .bind(corpus_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn pending_artifacts_for_corpus(&self, corpus_id: &str) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE corpus_id = ? AND embed_state = 'pending' ORDER BY ordinal",
        )
        .bind(corpus_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

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

    pub async fn update_artifact_category(&self, id: &str, category: Option<&str>) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET category = ? WHERE id = ?")
                .bind(category)
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn update_artifact_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET tags = ? WHERE id = ?")
                .bind(serde_json::to_string(tags).unwrap_or_else(|_| "[]".into()))
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

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

    pub async fn set_superseded_by(&self, artifact_id: &str, by: Option<&str>) -> Result<()> {
        let status = if by.is_some() {
            ArtifactStatus::Superseded
        } else {
            ArtifactStatus::Active
        };
        let res = sqlx::query("UPDATE artifacts SET superseded_by = ?, status = ? WHERE id = ?")
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

    pub async fn set_artifact_status(&self, id: &str, status: ArtifactStatus) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET status = ? WHERE id = ?")
                .bind(status.as_str())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn set_last_verified_at(&self, id: &str, at: i64) -> Result<()> {
        self.expect_updated(
            sqlx::query("UPDATE artifacts SET last_verified_at = ? WHERE id = ?")
                .bind(at)
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn list_non_active_artifacts(&self, limit: usize) -> Result<Vec<Chunk>> {
        let rows = sqlx::query(
            "SELECT * FROM artifacts WHERE status != 'active' OR superseded_by IS NOT NULL
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_artifact).collect())
    }

    pub async fn list_all_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    pub async fn list_embedded_artifact_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM artifacts WHERE embed_state = 'embedded'")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

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
    async fn the_fts_index_is_gone() {
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
