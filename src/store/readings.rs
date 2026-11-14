//! What the decision model read off each note: one row per artifact and question, the newest kept.
//! A reading describes; it is not a decision, so it is never banded, logged or put to a person.

use super::{Store, now};
use crate::core::judging::Asked;
use crate::error::Result;
use crate::infer::decide::Answer;

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub question_id: String,
    pub question_version: u32,
    pub answer: Answer,
    pub model: String,
    pub at: i64,
}

impl Store {
    pub async fn put_readings(
        &self,
        artifact_id: &str,
        rows: &[(&Asked, &Answer)],
        model: &str,
    ) -> Result<()> {
        let at = now();
        let mut tx = self.pool.begin().await?;
        for (a, answer) in rows {
            let json = serde_json::to_string(answer)
                .map_err(|e| crate::error::Error::Store(e.to_string()))?;
            sqlx::query!(
                "INSERT INTO artifact_readings (artifact_id, question_id, question_version, answer, model, at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT (artifact_id, question_id) DO UPDATE SET
                   question_version = excluded.question_version, answer = excluded.answer,
                   model = excluded.model, at = excluded.at",
                artifact_id,
                a.id,
                a.version,
                json,
                model,
                at
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn readings_of(&self, artifact_id: &str) -> Result<Vec<Reading>> {
        let rows = sqlx::query!(
            r#"SELECT question_id, question_version, answer, model, at FROM artifact_readings
                WHERE artifact_id = ? ORDER BY question_id"#,
            artifact_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            // An answer this binary cannot read is no reading.
            .filter_map(|r| {
                Some(Reading {
                    answer: serde_json::from_str(&r.answer).ok()?,
                    question_id: r.question_id,
                    question_version: u32::try_from(r.question_version).ok()?,
                    model: r.model,
                    at: r.at,
                })
            })
            .collect())
    }

    /// The yes of a noul reading, when the artifact has one at this version.
    pub async fn reading_yes(&self, artifact_id: &str, a: &Asked) -> Result<Option<f64>> {
        let answer = sqlx::query_scalar!(
            "SELECT answer FROM artifact_readings
              WHERE artifact_id = ? AND question_id = ? AND question_version = ?",
            artifact_id,
            a.id,
            a.version
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(match answer.and_then(|j| serde_json::from_str(&j).ok()) {
            Some(Answer::Noul { yes }) => Some(yes),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::judging::catalogue::{category, pinned, self_contained};
    use crate::store::artifacts::NewArtifact;

    async fn one_artifact(store: &Store) -> String {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    text: "a note".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    fn noul(yes: f64) -> Answer {
        Answer::Noul { yes }
    }

    #[tokio::test]
    async fn readings_are_kept_newest_per_question() {
        let store = Store::memory().await.unwrap();
        let id = one_artifact(&store).await;
        let (p, s, c) = (pinned(), self_contained(), category());
        let kind = Answer::Choice {
            pick: "procedure".into(),
            probabilities: vec![0.1, 0.6, 0.1, 0.05, 0.05, 0.05, 0.03, 0.02],
            confidence: 0.6,
        };
        store
            .put_readings(&id, &[(&p, &noul(0.2)), (&s, &noul(0.7))], "m1")
            .await
            .unwrap();
        store
            .put_readings(&id, &[(&p, &noul(0.95)), (&c, &kind)], "m2")
            .await
            .unwrap();
        let mut got = store.readings_of(&id).await.unwrap();
        got.sort_by(|a, b| a.question_id.cmp(&b.question_id));
        let by: Vec<(&str, &Answer, &str)> = got
            .iter()
            .map(|r| (r.question_id.as_str(), &r.answer, r.model.as_str()))
            .collect();
        assert_eq!(
            by,
            [
                ("category", &kind, "m2"),
                ("pinned", &noul(0.95), "m2"),
                ("self_contained", &noul(0.7), "m1"),
            ]
        );
        assert!(got.iter().all(|r| r.question_version == 1 && r.at > 0));
    }

    #[tokio::test]
    async fn a_readings_yes_is_found_only_at_the_current_version() {
        let store = Store::memory().await.unwrap();
        let id = one_artifact(&store).await;
        let p = pinned();
        assert_eq!(store.reading_yes(&id, &p).await.unwrap(), None);
        store
            .put_readings(&id, &[(&p, &noul(0.93))], "m")
            .await
            .unwrap();
        assert_eq!(store.reading_yes(&id, &p).await.unwrap(), Some(0.93));
        let mut next = pinned();
        next.version = 2;
        assert_eq!(store.reading_yes(&id, &next).await.unwrap(), None);
        // A choice has no yes.
        let c = category();
        let kind = Answer::Choice {
            pick: "concept".into(),
            probabilities: vec![0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            confidence: 0.9,
        };
        store.put_readings(&id, &[(&c, &kind)], "m").await.unwrap();
        assert_eq!(store.reading_yes(&id, &c).await.unwrap(), None);
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_readings() {
        let store = Store::memory().await.unwrap();
        let id = one_artifact(&store).await;
        let p = pinned();
        store
            .put_readings(&id, &[(&p, &noul(0.5))], "m")
            .await
            .unwrap();
        assert_eq!(store.readings_of(&id).await.unwrap().len(), 1);
        store.delete_artifact(&id).await.unwrap();
        assert!(store.readings_of(&id).await.unwrap().is_empty());
        let left: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM artifact_readings")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
