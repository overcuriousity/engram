//! What the decision model said of notes a search did not show, one source at a time (a gap, a
//! weak decision, a labelled search). A row with `artifact_id = ''` marks the look as done.

use super::gaps::GapKind;
use super::search_labels::LABEL_GIVE_UP;
use super::{Store, now};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeepKind {
    Gap(GapKind),
    WeakRight,
    SecondReading,
    Labelled,
}

impl DeepKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeepKind::Gap(k) => k.as_str(),
            DeepKind::WeakRight => "weak_right",
            DeepKind::SecondReading => "second_reading",
            DeepKind::Labelled => "labelled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "weak_right" => Some(DeepKind::WeakRight),
            "second_reading" => Some(DeepKind::SecondReading),
            "labelled" => Some(DeepKind::Labelled),
            other => GapKind::parse(other).map(DeepKind::Gap),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeepSource {
    pub kind: DeepKind,
    pub id: String,
}

impl DeepSource {
    /// The queue's target: `kind:id`.
    pub fn target(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }

    /// Splits at the first `:`; a kind has none, an id may.
    pub fn parse(target: &str) -> Option<Self> {
        let (kind, id) = target.split_once(':')?;
        Some(Self {
            kind: DeepKind::parse(kind)?,
            id: id.to_string(),
        })
    }
}

impl Store {
    /// Whether this source's look is done in this era.
    pub async fn deep_looked(&self, s: &DeepSource, version: u32, model: &str) -> Result<bool> {
        let kind = s.kind.as_str();
        Ok(sqlx::query_scalar!(
            "SELECT 1 FROM deep_looks
              WHERE source_kind = ? AND source_id = ? AND artifact_id = ''
                AND question_version = ? AND model = ?",
            kind,
            s.id,
            version,
            model
        )
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    /// The notes already asked about for this source in this era.
    pub async fn deep_asked(
        &self,
        s: &DeepSource,
        version: u32,
        model: &str,
    ) -> Result<Vec<String>> {
        let kind = s.kind.as_str();
        Ok(sqlx::query_scalar!(
            "SELECT artifact_id FROM deep_looks
              WHERE source_kind = ? AND source_id = ? AND artifact_id <> ''
                AND question_version = ? AND model = ?",
            kind,
            s.id,
            version,
            model
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every answer, then the done row, in one transaction. Looked at, its failed attempts are
    /// done with.
    pub async fn put_deep_look(
        &self,
        s: &DeepSource,
        answers: &[(String, f64)],
        version: u32,
        model: &str,
    ) -> Result<()> {
        let kind = s.kind.as_str();
        let at = now();
        let mut tx = self.pool.begin().await?;
        for (artifact_id, yes) in answers
            .iter()
            .map(|(a, y)| (a.as_str(), *y))
            .chain([("", 0.0)])
        {
            sqlx::query!(
                "INSERT INTO deep_looks
                   (source_kind, source_id, artifact_id, yes, question_version, model, at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(source_kind, source_id, artifact_id) DO UPDATE SET
                   yes = excluded.yes, question_version = excluded.question_version,
                   model = excluded.model, at = excluded.at",
                kind,
                s.id,
                artifact_id,
                yes,
                version,
                model,
                at
            )
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query!(
            "DELETE FROM deep_look_failures WHERE source_kind = ? AND source_id = ?",
            kind,
            s.id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// As `label_failed`: not due before `retry_at`, counted when the model `refused`, started
    /// over by a newer era. Returns the refusals so far in this era.
    pub async fn deep_look_failed(
        &self,
        s: &DeepSource,
        version: u32,
        model: &str,
        refused: bool,
        retry_at: i64,
    ) -> Result<i64> {
        let kind = s.kind.as_str();
        let refused = i64::from(refused);
        sqlx::query!(
            "INSERT INTO deep_look_failures
               (source_kind, source_id, question_version, model, refused, retry_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(source_kind, source_id) DO UPDATE SET
               refused = CASE WHEN question_version = excluded.question_version
                               AND model = excluded.model
                              THEN refused + excluded.refused ELSE excluded.refused END,
               question_version = excluded.question_version,
               model = excluded.model,
               retry_at = excluded.retry_at",
            kind,
            s.id,
            version,
            model,
            refused,
            retry_at
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_scalar!(
            "SELECT refused FROM deep_look_failures WHERE source_kind = ? AND source_id = ?",
            kind,
            s.id
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// No failure in this era from `LABEL_GIVE_UP` refusals on, or still waiting out a retry.
    pub async fn deep_look_due(&self, s: &DeepSource, version: u32, model: &str) -> Result<bool> {
        let kind = s.kind.as_str();
        let now = now();
        Ok(sqlx::query_scalar!(
            "SELECT 1 FROM deep_look_failures
              WHERE source_kind = ? AND source_id = ? AND question_version = ? AND model = ?
                AND (refused >= ? OR retry_at > ?)",
            kind,
            s.id,
            version,
            model,
            LABEL_GIVE_UP,
            now
        )
        .fetch_optional(&self.pool)
        .await?
        .is_none())
    }

    /// Sources looked at in this era, and of those the ones with a note at or above the line.
    pub async fn deep_counts(
        &self,
        relevant_at: f64,
        version: u32,
        model: &str,
    ) -> Result<(i64, i64)> {
        let row = sqlx::query!(
            r#"SELECT COUNT(*) AS "looks!: i64",
                      COALESCE(SUM(EXISTS (
                        SELECT 1 FROM deep_looks f
                         WHERE f.source_kind = d.source_kind AND f.source_id = d.source_id
                           AND f.artifact_id <> '' AND f.yes >= ?
                           AND f.question_version = ? AND f.model = ?)), 0) AS "found!: i64"
                 FROM deep_looks d
                WHERE d.artifact_id = '' AND d.question_version = ? AND d.model = ?"#,
            relevant_at,
            version,
            model,
            version,
            model
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((row.looks, row.found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::search_labels::tests::{notes, search};

    const V: u32 = 1;
    const M: &str = "clef";

    fn src(kind: DeepKind, id: &str) -> DeepSource {
        DeepSource {
            kind,
            id: id.into(),
        }
    }

    #[tokio::test]
    async fn a_look_is_done_once_put_and_only_in_its_era() {
        let store = Store::memory().await.unwrap();
        let s = src(DeepKind::Labelled, "e1");
        assert!(!store.deep_looked(&s, V, M).await.unwrap());
        store
            .put_deep_look(&s, &[("a".into(), 0.1), ("b".into(), 0.9)], V, M)
            .await
            .unwrap();
        assert!(store.deep_looked(&s, V, M).await.unwrap());
        let mut asked = store.deep_asked(&s, V, M).await.unwrap();
        asked.sort();
        assert_eq!(asked, ["a", "b"]);
        assert!(!store.deep_looked(&s, V + 1, M).await.unwrap());
        assert!(!store.deep_looked(&s, V, "other").await.unwrap());
        assert_eq!(store.deep_counts(0.5, V, M).await.unwrap(), (1, 1));
        assert_eq!(store.deep_counts(0.95, V, M).await.unwrap(), (1, 0));
    }

    #[tokio::test]
    async fn refusals_past_the_give_up_make_a_look_not_due() {
        let store = Store::memory().await.unwrap();
        let s = src(DeepKind::WeakRight, "d1");
        assert!(store.deep_look_due(&s, V, M).await.unwrap());
        for _ in 0..LABEL_GIVE_UP {
            store.deep_look_failed(&s, V, M, true, 0).await.unwrap();
        }
        assert!(!store.deep_look_due(&s, V, M).await.unwrap());
        assert!(store.deep_look_due(&s, V + 1, M).await.unwrap());
        store.put_deep_look(&s, &[], V, M).await.unwrap();
        assert!(store.deep_look_due(&s, V, M).await.unwrap());
    }

    #[test]
    fn a_target_parses_back_for_every_kind() {
        let kinds = [
            DeepKind::Gap(GapKind::Ask),
            DeepKind::Gap(GapKind::Search),
            DeepKind::Gap(GapKind::Unmatched),
            DeepKind::Gap(GapKind::Pursuit),
            DeepKind::Gap(GapKind::Subject),
            DeepKind::Gap(GapKind::Decide),
            DeepKind::WeakRight,
            DeepKind::SecondReading,
            DeepKind::Labelled,
        ];
        for kind in kinds {
            let s = src(kind, "id:with:colons");
            assert_eq!(DeepSource::parse(&s.target()), Some(s));
        }
        assert_eq!(DeepSource::parse("nonsense:1"), None);
    }

    #[tokio::test]
    async fn trimming_removes_the_rows_of_a_deleted_search() {
        let store = Store::memory().await.unwrap();
        let n = notes(&store, 1).await;
        let kept = search(&store, "kept", &n).await;
        let gone = search(&store, "gone", &n).await;
        for id in [&kept, &gone] {
            let s = src(DeepKind::Labelled, id);
            store
                .put_deep_look(&s, &[(n[0].clone(), 0.9)], V, M)
                .await
                .unwrap();
            store.deep_look_failed(&s, V, M, true, 0).await.unwrap();
        }
        sqlx::query!("DELETE FROM search_events WHERE id = ?", gone)
            .execute(&store.pool)
            .await
            .unwrap();
        store.trim_gap_coverage(0.0).await.unwrap();
        assert_eq!(store.deep_counts(0.5, V, M).await.unwrap(), (1, 1));
        let left = sqlx::query_scalar!("SELECT COUNT(*) FROM deep_look_failures")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }
}
