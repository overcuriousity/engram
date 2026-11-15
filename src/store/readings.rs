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

/// How the reader's last visit to a note went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit {
    Read,
    /// Not one character of it fits beside the questions.
    NoRoom,
    /// The model gave no answer to store; asked again after `retry_at`.
    Rejected,
}

impl Visit {
    pub fn as_str(self) -> &'static str {
        match self {
            Visit::Read => "read",
            Visit::NoRoom => "no_room",
            Visit::Rejected => "rejected",
        }
    }

    fn parse(s: &str) -> Option<Visit> {
        match s {
            "read" => Some(Visit::Read),
            "no_room" => Some(Visit::NoRoom),
            "rejected" => Some(Visit::Rejected),
            _ => None,
        }
    }
}

/// The reader's last visit to a note. A mark about other text or other questions says nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadMark {
    /// Of the title and the text.
    pub text_hash: String,
    /// `asked_key` of the questions put.
    pub asked: String,
    /// The tokens the note had to fit in.
    pub room: i64,
    pub visit: Visit,
    /// Rejected visits in a row.
    pub tries: i64,
    pub retry_at: i64,
}

/// One JSON object of question id to version: what the SQL walks, and what a mark is keyed on.
pub fn asked_key(versions: &[(&str, u32)]) -> String {
    let asked: serde_json::Map<String, serde_json::Value> = versions
        .iter()
        .map(|(id, v)| ((*id).to_string(), (*v).into()))
        .collect();
    serde_json::Value::Object(asked).to_string()
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

    /// Live, embedded artifacts the reader should visit, newest first: one with no reading at some
    /// question's current version, or one rejected whose wait is over. One the last visit could
    /// not read is left out while its text, the questions and (for no room) `room` are the same.
    pub async fn artifacts_lacking_readings(
        &self,
        versions: &[(&str, u32)],
        room: i64,
        limit: i64,
    ) -> Result<Vec<String>> {
        let asked = asked_key(versions);
        let at = now();
        // A mark older than the text's last change is about other text and is not joined.
        Ok(sqlx::query_scalar!(
            r#"SELECT a.id AS "id!" FROM artifacts a
                 LEFT JOIN artifact_reads m
                   ON m.artifact_id = a.id AND m.asked = ?1 AND m.at >= a.updated_at
                WHERE a.status = 'active' AND a.superseded_by IS NULL
                  AND a.embed_state = 'embedded'
                  AND CASE m.visit
                        WHEN 'no_room' THEN m.room <> ?2
                        WHEN 'rejected' THEN m.retry_at <= ?3
                        ELSE EXISTS (SELECT 1 FROM json_each(?1) q
                                      WHERE NOT EXISTS (SELECT 1 FROM artifact_readings r
                                                         WHERE r.artifact_id = a.id
                                                           AND r.question_id = q.key
                                                           AND r.question_version = q.value))
                      END
                ORDER BY a.created_at DESC, a.id DESC LIMIT ?4"#,
            asked,
            room,
            at,
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn put_read_mark(&self, artifact_id: &str, mark: &ReadMark) -> Result<()> {
        let (visit, at) = (mark.visit.as_str(), now());
        sqlx::query!(
            "INSERT INTO artifact_reads (artifact_id, text_hash, asked, room, visit, tries, retry_at, at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (artifact_id) DO UPDATE SET
               text_hash = excluded.text_hash, asked = excluded.asked, room = excluded.room,
               visit = excluded.visit, tries = excluded.tries, retry_at = excluded.retry_at,
               at = excluded.at",
            artifact_id,
            mark.text_hash,
            mark.asked,
            mark.room,
            visit,
            mark.tries,
            mark.retry_at,
            at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn read_mark(&self, artifact_id: &str) -> Result<Option<ReadMark>> {
        let row = sqlx::query!(
            "SELECT text_hash, asked, room, visit, tries, retry_at FROM artifact_reads
              WHERE artifact_id = ?",
            artifact_id
        )
        .fetch_optional(&self.pool)
        .await?;
        // A visit this binary cannot name is no mark.
        Ok(row.and_then(|r| {
            Some(ReadMark {
                visit: Visit::parse(&r.visit)?,
                text_hash: r.text_hash,
                asked: r.asked,
                room: r.room,
                tries: r.tries,
                retry_at: r.retry_at,
            })
        }))
    }

    /// Whether every question in `asked` has a reading at its version no older than the note's
    /// text: how readings from before visits were marked are told from stale ones.
    pub async fn readings_current(&self, artifact_id: &str, asked: &str) -> Result<bool> {
        let lacking = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM json_each(?1) q
                WHERE NOT EXISTS (SELECT 1 FROM artifact_readings r
                                    JOIN artifacts a ON a.id = r.artifact_id
                                   WHERE r.artifact_id = ?2 AND r.question_id = q.key
                                     AND r.question_version = q.value
                                     AND r.at >= a.updated_at)"#,
            asked,
            artifact_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(lacking == 0)
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

    const ROOM: i64 = 1_000;

    /// As after its embed, created at `at`.
    async fn embedded(store: &Store, id: &str, at: i64) {
        sqlx::query!(
            "UPDATE artifacts SET created_at = ?, embed_state = 'embedded' WHERE id = ?",
            at,
            id
        )
        .execute(&store.pool)
        .await
        .unwrap();
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
    async fn a_note_already_read_at_this_version_is_not_read_again_and_a_new_version_reads_all_again()
     {
        let store = Store::memory().await.unwrap();
        let (old, new, gone) = (
            one_artifact(&store).await,
            one_artifact(&store).await,
            one_artifact(&store).await,
        );
        for (id, at) in [(&old, 100), (&new, 300), (&gone, 200)] {
            embedded(&store, id, at).await;
        }
        store.trash_artifact(&gone, 400).await.unwrap();
        let asked = crate::jobs::read::reading_questions();
        let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap(),
            [new.clone(), old.clone()],
            "newest first, and a note out of results is not read"
        );
        let yes = noul(0.5);
        let rows: Vec<(&Asked, &Answer)> = asked.iter().map(|a| (a, &yes)).collect();
        store.put_readings(&old, &rows, "m").await.unwrap();
        // Three of four is not read.
        store.put_readings(&new, &rows[..3], "m").await.unwrap();
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap(),
            std::slice::from_ref(&new)
        );
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 1)
                .await
                .unwrap(),
            std::slice::from_ref(&new)
        );
        let mut next = versions.clone();
        next[2].1 = 2;
        assert_eq!(
            store
                .artifacts_lacking_readings(&next, ROOM, 10)
                .await
                .unwrap(),
            [new, old],
            "a question at a new version reads every note again"
        );
    }

    #[tokio::test]
    async fn a_note_still_to_be_embedded_is_left_to_its_own_read() {
        let store = Store::memory().await.unwrap();
        let (waiting, done) = (one_artifact(&store).await, one_artifact(&store).await);
        embedded(&store, &done, 100).await;
        let asked = crate::jobs::read::reading_questions();
        let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap(),
            std::slice::from_ref(&done)
        );
        embedded(&store, &waiting, 200).await;
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap(),
            [waiting, done]
        );
    }

    fn mark(asked: &str, visit: Visit, retry_at: i64) -> ReadMark {
        ReadMark {
            text_hash: "h".into(),
            asked: asked.into(),
            room: ROOM,
            visit,
            tries: 1,
            retry_at,
        }
    }

    #[tokio::test]
    async fn a_note_that_could_not_be_read_is_left_out_until_something_about_it_changes() {
        let store = Store::memory().await.unwrap();
        let (tight, refused, plain) = (
            one_artifact(&store).await,
            one_artifact(&store).await,
            one_artifact(&store).await,
        );
        for (id, at) in [(&tight, 300), (&refused, 200), (&plain, 100)] {
            embedded(&store, id, at).await;
        }
        let asked = crate::jobs::read::reading_questions();
        let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
        let key = asked_key(&versions);
        let lacking = |versions: Vec<(&'static str, u32)>, room: i64| {
            let store = store.clone();
            async move {
                store
                    .artifacts_lacking_readings(&versions, room, 10)
                    .await
                    .unwrap()
            }
        };
        store
            .put_read_mark(&tight, &mark(&key, Visit::NoRoom, 0))
            .await
            .unwrap();
        store
            .put_read_mark(&refused, &mark(&key, Visit::Rejected, now() + 600))
            .await
            .unwrap();
        assert_eq!(
            store.read_mark(&refused).await.unwrap().unwrap().visit,
            Visit::Rejected
        );
        assert_eq!(
            lacking(versions.clone(), ROOM).await,
            std::slice::from_ref(&plain),
            "neither holds a place in the list"
        );
        assert_eq!(
            lacking(versions.clone(), ROOM + 1).await,
            [tight.clone(), plain.clone()],
            "more room is another chance for the one that had none"
        );
        let mut next = versions.clone();
        next[0].1 += 1;
        assert_eq!(
            lacking(next, ROOM).await,
            [tight.clone(), refused.clone(), plain.clone()],
            "a question at a new version tries every note again"
        );

        store
            .put_read_mark(&refused, &mark(&key, Visit::Rejected, now() - 1))
            .await
            .unwrap();
        assert_eq!(
            lacking(versions.clone(), ROOM).await,
            [refused.clone(), plain.clone()],
            "its wait is over"
        );
        store
            .put_read_mark(&refused, &mark(&key, Visit::Rejected, now() + 600))
            .await
            .unwrap();

        // The text changed after the visit, so the mark is about other text.
        let later = now() + 60;
        sqlx::query!(
            "UPDATE artifacts SET updated_at = ? WHERE id = ?",
            later,
            &tight
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(lacking(versions, ROOM).await, [tight, plain]);
    }

    #[tokio::test]
    async fn a_rejected_note_that_has_older_readings_is_still_tried_again_when_its_wait_is_over() {
        let store = Store::memory().await.unwrap();
        let id = one_artifact(&store).await;
        embedded(&store, &id, 100).await;
        let asked = crate::jobs::read::reading_questions();
        let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
        let key = asked_key(&versions);
        let yes = noul(0.5);
        let rows: Vec<(&Asked, &Answer)> = asked.iter().map(|a| (a, &yes)).collect();
        store.put_readings(&id, &rows, "m").await.unwrap();
        store
            .put_read_mark(&id, &mark(&key, Visit::Rejected, now() + 600))
            .await
            .unwrap();
        assert!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .put_read_mark(&id, &mark(&key, Visit::Rejected, now() - 1))
            .await
            .unwrap();
        assert_eq!(
            store
                .artifacts_lacking_readings(&versions, ROOM, 10)
                .await
                .unwrap(),
            [id]
        );
    }

    #[tokio::test]
    async fn readings_stand_while_they_are_no_older_than_the_text() {
        let store = Store::memory().await.unwrap();
        let id = one_artifact(&store).await;
        let asked = crate::jobs::read::reading_questions();
        let versions: Vec<(&str, u32)> = asked.iter().map(|a| (a.id, a.version)).collect();
        let key = asked_key(&versions);
        assert!(!store.readings_current(&id, &key).await.unwrap());
        let yes = noul(0.5);
        let rows: Vec<(&Asked, &Answer)> = asked.iter().map(|a| (a, &yes)).collect();
        store.put_readings(&id, &rows[..3], "m").await.unwrap();
        assert!(
            !store.readings_current(&id, &key).await.unwrap(),
            "three of four"
        );
        store.put_readings(&id, &rows, "m").await.unwrap();
        assert!(store.readings_current(&id, &key).await.unwrap());
        let mut next = versions.clone();
        next[1].1 += 1;
        assert!(
            !store
                .readings_current(&id, &asked_key(&next))
                .await
                .unwrap()
        );
        let later = now() + 60;
        sqlx::query!(
            "UPDATE artifacts SET updated_at = ? WHERE id = ?",
            later,
            &id
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert!(
            !store.readings_current(&id, &key).await.unwrap(),
            "the text is newer than its readings"
        );
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
