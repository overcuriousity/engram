//! Recorded decisions: the passages read and what each said, which cannot be rebuilt later.

use super::asks::AskVerdict;
use super::{Store, new_id, now};
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct NewEvidence {
    /// Retrieval order.
    pub rank: usize,
    pub artifact_id: String,
    pub bears: f64,
    pub bearing: bool,
    /// Went into the joint read.
    pub joint: bool,
    /// Place in the evidence; `None` when it did not bear.
    pub n: Option<usize>,
    /// JSON, `{key: answer}`.
    pub read: String,
}

#[derive(Debug, Clone)]
pub struct NewDecision {
    pub question: String,
    pub questions: String,
    pub situation: Option<String>,
    pub scope: Option<String>,
    pub filters: String,
    /// `None` when not held.
    pub answers: Option<String>,
    pub prior: String,
    pub standing: String,
    pub not_held: bool,
    pub model: String,
    pub read: usize,
    pub dropped: usize,
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub evidence: Vec<NewEvidence>,
}

#[derive(Debug, Clone)]
pub struct StoredEvidence {
    pub rank: i64,
    pub artifact_id: String,
    pub bears: f64,
    pub bearing: bool,
    pub joint: bool,
    pub n: Option<i64>,
    pub read: String,
}

#[derive(Debug, Clone)]
pub struct StoredDecision {
    pub id: String,
    pub question: String,
    pub questions: String,
    pub answers: Option<String>,
    pub standing: String,
    pub not_held: bool,
    pub created_at: i64,
    pub verdict: Option<AskVerdict>,
    pub correct: Option<String>,
    pub evidence: Vec<StoredEvidence>,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

impl Store {
    pub async fn record_decision(&self, d: NewDecision) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let id = new_id();
        sqlx::query!(
            "INSERT INTO decide_events
               (id, question, questions, situation, scope, filters, answers, prior, standing,
                not_held, model, read, dropped, query_vec, vec_dim, embed_model, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &id,
            &d.question,
            &d.questions,
            &d.situation,
            &d.scope,
            &d.filters,
            &d.answers,
            &d.prior,
            &d.standing,
            d.not_held as i64,
            &d.model,
            d.read as i64,
            d.dropped as i64,
            vec_to_blob(&d.query_vec),
            d.query_vec.len() as i64,
            &d.embed_model,
            now()
        )
        .execute(&mut *tx)
        .await?;
        for e in &d.evidence {
            let (rank, n) = (e.rank as i64, e.n.map(|n| n as i64));
            sqlx::query!(
                "INSERT INTO decide_evidence (event_id, rank, artifact_id, bears, bearing, joint, n, read)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                &id,
                rank,
                &e.artifact_id,
                e.bears,
                e.bearing as i64,
                e.joint as i64,
                n,
                &e.read
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn decision(&self, id: &str) -> Result<Option<StoredDecision>> {
        let Some(r) = sqlx::query!(
            r#"SELECT id AS "id!", question, questions, answers, standing, not_held, created_at,
                      verdict, correct
                 FROM decide_events WHERE id = ?"#,
            id
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let evidence = sqlx::query!(
            "SELECT rank, artifact_id, bears, bearing, joint, n, read
               FROM decide_evidence WHERE event_id = ? ORDER BY rank",
            id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|e| StoredEvidence {
            rank: e.rank,
            artifact_id: e.artifact_id,
            bears: e.bears,
            bearing: e.bearing != 0,
            joint: e.joint != 0,
            n: e.n,
            read: e.read,
        })
        .collect();
        Ok(Some(StoredDecision {
            id: r.id,
            question: r.question,
            questions: r.questions,
            answers: r.answers,
            standing: r.standing,
            not_held: r.not_held != 0,
            created_at: r.created_at,
            verdict: r.verdict.as_deref().and_then(AskVerdict::parse),
            correct: r.correct,
            evidence,
        }))
    }

    pub async fn judge_decision(
        &self,
        id: &str,
        verdict: AskVerdict,
        correct: Option<&str>,
    ) -> Result<()> {
        let (at, v) = (now(), verdict.as_str());
        let res = sqlx::query!(
            "UPDATE decide_events SET judged_at = ?, verdict = ?, correct = ? WHERE id = ?",
            at,
            v,
            correct,
            id
        )
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn unjudge_decision(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE decide_events SET judged_at = NULL, verdict = NULL, correct = NULL WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A judged row is the person's record and stays.
    pub async fn expire_decisions(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(sqlx::query!(
            "DELETE FROM decide_events WHERE created_at < ? AND verdict IS NULL",
            now() - retain_days * 86_400
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// `decide_evidence` goes through ON DELETE CASCADE.
    pub async fn purge_decisions(&self) -> Result<u64> {
        Ok(sqlx::query!("DELETE FROM decide_events")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;
    use crate::store::asks::AskVerdict;
    use crate::store::{Store, now};

    fn evidence(rank: usize, bearing: bool, n: Option<usize>) -> NewEvidence {
        NewEvidence {
            rank,
            artifact_id: format!("art-{rank}"),
            bears: 0.9 - rank as f64 * 0.1,
            bearing,
            joint: bearing,
            n,
            read: r#"{"c":"keep"}"#.into(),
        }
    }

    fn sample() -> NewDecision {
        NewDecision {
            question: "Keep bge-m3?".into(),
            questions: r#"{"c":{"type":"noul"}}"#.into(),
            situation: Some("12 GB.".into()),
            scope: Some("me".into()),
            filters: "{}".into(),
            answers: Some(r#"{"c":{"yes":0.8}}"#.into()),
            prior: r#"{"c":{"yes":0.5}}"#.into(),
            standing: r#"{"c":"settled"}"#.into(),
            not_held: false,
            model: "clef".into(),
            read: 3,
            dropped: 1,
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            evidence: vec![
                evidence(0, true, Some(1)),
                evidence(1, false, None),
                evidence(2, true, Some(2)),
            ],
        }
    }

    #[tokio::test]
    async fn a_decision_is_read_back_as_recorded() {
        let s = Store::memory().await.unwrap();
        let id = s.record_decision(sample()).await.unwrap();
        let d = s.decision(&id).await.unwrap().unwrap();
        let w = sample();
        assert_eq!(d.id, id);
        assert_eq!(d.question, w.question);
        assert_eq!(d.questions, w.questions);
        assert_eq!(d.answers, w.answers);
        assert_eq!(d.standing, w.standing);
        assert!(!d.not_held);
        assert!(d.created_at > 0);
        assert_eq!((d.verdict, d.correct), (None, None));
        let got: Vec<_> = d
            .evidence
            .iter()
            .map(|e| (e.rank, e.artifact_id.as_str(), e.bearing, e.joint, e.n))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "art-0", true, true, Some(1)),
                (1, "art-1", false, false, None),
                (2, "art-2", true, true, Some(2)),
            ]
        );
        assert_eq!(d.evidence[0].read, r#"{"c":"keep"}"#);
        assert!((d.evidence[2].bears - 0.7).abs() < 1e-9);
        assert!(s.decision("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_not_held_decision_has_no_answers_and_keeps_its_reads() {
        let s = Store::memory().await.unwrap();
        let mut n = sample();
        n.answers = None;
        n.not_held = true;
        n.evidence = vec![evidence(0, false, None), evidence(1, false, None)];
        let id = s.record_decision(n).await.unwrap();
        let d = s.decision(&id).await.unwrap().unwrap();
        assert!(d.answers.is_none() && d.not_held);
        assert_eq!(d.evidence.len(), 2);
        assert!(d.evidence.iter().all(|e| !e.bearing));
    }

    #[tokio::test]
    async fn a_verdict_is_set_and_undone() {
        let s = Store::memory().await.unwrap();
        let id = s.record_decision(sample()).await.unwrap();
        s.judge_decision(&id, AskVerdict::Wrong, Some(r#"{"c":"no"}"#))
            .await
            .unwrap();
        let d = s.decision(&id).await.unwrap().unwrap();
        assert_eq!(d.verdict, Some(AskVerdict::Wrong));
        assert_eq!(d.correct.as_deref(), Some(r#"{"c":"no"}"#));
        s.unjudge_decision(&id).await.unwrap();
        let d = s.decision(&id).await.unwrap().unwrap();
        assert_eq!((d.verdict, d.correct), (None, None));
    }

    #[tokio::test]
    async fn judging_an_unknown_decision_is_not_found() {
        let s = Store::memory().await.unwrap();
        let r = s.judge_decision("nope", AskVerdict::Right, None).await;
        assert!(matches!(r, Err(crate::error::Error::NotFound)));
    }

    #[tokio::test]
    async fn old_unjudged_decisions_expire_and_judged_ones_stay() {
        let s = Store::memory().await.unwrap();
        let old = s.record_decision(sample()).await.unwrap();
        let judged = s.record_decision(sample()).await.unwrap();
        let fresh = s.record_decision(sample()).await.unwrap();
        s.judge_decision(&judged, AskVerdict::Right, None)
            .await
            .unwrap();
        let back = now() - 40 * 86_400;
        for id in [&old, &judged] {
            // runtime: a test fixture backdating a row.
            sqlx::query("UPDATE decide_events SET created_at = ? WHERE id = ?")
                .bind(back)
                .bind(id)
                .execute(&s.pool)
                .await
                .unwrap();
        }
        assert_eq!(s.expire_decisions(0).await.unwrap(), 0);
        assert_eq!(s.expire_decisions(30).await.unwrap(), 1);
        assert!(s.decision(&old).await.unwrap().is_none());
        assert!(s.decision(&judged).await.unwrap().is_some());
        assert!(s.decision(&fresh).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn purging_takes_the_evidence_with_it() {
        let s = Store::memory().await.unwrap();
        s.record_decision(sample()).await.unwrap();
        assert_eq!(s.purge_decisions().await.unwrap(), 1);
        // runtime: a test fixture counting rows.
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decide_evidence")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn created_at_among_dates_the_artifacts_it_knows() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        let ids = vec![made[0].id.clone(), "unknown".to_string()];
        let m = s.created_at_among(&ids).await.unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[&made[0].id], made[0].created_at);
        assert!(s.created_at_among(&[]).await.unwrap().is_empty());
    }
}
