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
    /// Nothing reached the line; the closest were weighed.
    pub weak: bool,
    /// As the server named itself.
    pub model: String,
    /// The configured decision model.
    pub decider: String,
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
    pub weak: bool,
    pub created_at: i64,
    pub verdict: Option<AskVerdict>,
    pub correct: Option<String>,
    pub evidence: Vec<StoredEvidence>,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

impl Store {
    /// A held decision's joint evidence is observed as cited, as an ask's used citations are; a weak one's is not.
    pub async fn record_decision(&self, d: NewDecision) -> Result<String> {
        // Read before the transaction opens, so the write lock is not held across another table.
        let generation = self.live_generation().await?;
        let mut tx = self.pool.begin().await?;
        let id = new_id();
        sqlx::query!(
            "INSERT INTO decide_events
               (id, question, questions, situation, scope, filters, answers, prior, standing,
                not_held, model, decider, read, dropped, query_vec, vec_dim, embed_model, created_at,
                bears_version, weak)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            &d.decider,
            d.read as i64,
            d.dropped as i64,
            vec_to_blob(&d.query_vec),
            d.query_vec.len() as i64,
            &d.embed_model,
            now(),
            crate::core::decide::state::BEARS_VERSION,
            d.weak as i64
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
            if let Some(g) = &generation
                && e.joint
                && !d.not_held
                && !d.weak
            {
                crate::store::observations::insert(
                    &mut *tx,
                    &crate::store::observations::NewObservation {
                        generation_id: g.id.clone(),
                        query: d.question.clone(),
                        query_vec: d.query_vec.clone(),
                        embed_model: d.embed_model.clone(),
                        artifact_id: Some(e.artifact_id.clone()),
                        rank: n,
                        source: crate::store::observations::Source::Cited,
                        event_id: None,
                    },
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn decision(&self, id: &str) -> Result<Option<StoredDecision>> {
        let Some(r) = sqlx::query!(
            r#"SELECT id AS "id!", question, questions, answers, standing, not_held, weak,
                      created_at, verdict, correct
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
            weak: r.weak != 0,
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

/// Of Decide's judged decisions, per standing: how many, and how many came out the way that says
/// the line is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecideShares {
    /// `split`, and of those judged right.
    pub split: (usize, usize),
    /// Held, and of those judged "nothing here".
    pub held: (usize, usize),
    /// Not held, and of those judged wrong.
    pub not_held: (usize, usize),
    /// Weak, and of those judged right.
    pub weak: (usize, usize),
}

/// What the verdicts say about Decide's bearing line since it last moved.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct BearsEvidence {
    /// Not weak, judged "nothing here": the line let notes in that bore nothing.
    pub too_low: usize,
    /// Weak, judged right: a note that answered was left under the line.
    pub too_high: usize,
    /// Not weak and right, or weak and "nothing here": the line did its work.
    pub fine: usize,
}

impl BearsEvidence {
    pub fn n(&self) -> usize {
        self.too_low + self.too_high + self.fine
    }
}

/// One decision as the tuner and Insights read it.
struct DecideRow {
    held: bool,
    weak: bool,
    /// The first question's standing.
    standing: Option<String>,
    verdict: Option<AskVerdict>,
}

impl Store {
    /// `model`'s decisions since the bearing line last moved, judged "right" or "nothing here"
    /// (a `wrong` verdict does not say where the line was, nor does a decision nothing held).
    pub async fn bears_verdicts(&self, model: &str) -> Result<BearsEvidence> {
        let era = crate::core::decide::settled_era(model);
        let since = self
            .moved_at(crate::core::decide::BEARS_ID, &era, false)
            .await?;
        let mut e = BearsEvidence::default();
        for r in self.decides_since(model, since).await? {
            match (r.held, r.weak, r.verdict) {
                (true, false, Some(AskVerdict::NothingHere)) => e.too_low += 1,
                (true, true, Some(AskVerdict::Right)) => e.too_high += 1,
                (true, false, Some(AskVerdict::Right))
                | (true, true, Some(AskVerdict::NothingHere)) => e.fine += 1,
                _ => {}
            }
        }
        Ok(e)
    }

    /// `bears_verdicts`, and when `with_readings` the unjudged held decisions since the line
    /// moved that a deep look has read again (a second reading, or a weak one looked at as a
    /// gap): what it said of their joint evidence stands in for a verdict. A verdict replaces it.
    pub async fn bears_evidence(
        &self,
        model: &str,
        relevant_at: f64,
        ask_at: f64,
        with_readings: bool,
    ) -> Result<BearsEvidence> {
        let mut e = self.bears_verdicts(model).await?;
        if !with_readings {
            return Ok(e);
        }
        let era = crate::core::decide::settled_era(model);
        let since = self
            .moved_at(crate::core::decide::BEARS_ID, &era, false)
            .await?;
        let version = crate::core::judging::catalogue::answers_query("").version;
        let rows = sqlx::query!(
            r#"SELECT e.weak AS "weak!: i64",
                      (SELECT MAX(d.yes) FROM decide_evidence v
                         JOIN deep_looks d ON d.source_id = v.event_id
                                          AND d.artifact_id = v.artifact_id
                        WHERE v.event_id = e.id AND v.joint = 1
                          AND d.source_kind = CASE e.weak WHEN 0 THEN 'second_reading'
                                                          ELSE 'decide' END
                          AND d.question_version = ? AND d.model = ?) AS "top: f64"
                 FROM decide_events e
                WHERE e.decider = ? AND e.created_at > ? AND COALESCE(e.bears_version, 1) = ?
                  AND e.not_held = 0 AND e.verdict IS NULL
                  AND EXISTS (SELECT 1 FROM deep_looks d
                               WHERE d.source_id = e.id AND d.artifact_id = ''
                                 AND d.question_version = ? AND d.model = ?
                                 AND d.source_kind = CASE e.weak WHEN 0 THEN 'second_reading'
                                                                 ELSE 'decide' END)"#,
            version,
            model,
            model,
            since,
            crate::core::decide::state::BEARS_VERSION,
            version,
            model
        )
        .fetch_all(&self.pool)
        .await?;
        for r in rows {
            let top = r.top.unwrap_or(0.0);
            match r.weak != 0 {
                false if top < ask_at => e.too_low += 1,
                true if top >= relevant_at => e.too_high += 1,
                _ => e.fine += 1,
            }
        }
        Ok(e)
    }

    /// Held `settled` decisions of `model` made since Decide's line last moved, judged right or
    /// wrong: (judged, wrong).
    pub async fn settled_record(&self, model: &str) -> Result<(usize, usize)> {
        let rows = self.decides_since_move(model).await?;
        // A weak decision stands at most "leaning", so it says nothing about the settled line.
        let settled = rows
            .iter()
            .filter(|r| r.held && !r.weak && r.standing.as_deref() == Some("settled"));
        let judged: Vec<_> = settled
            .filter_map(|r| r.verdict.filter(|v| *v != AskVerdict::NothingHere))
            .collect();
        let wrong = judged.iter().filter(|v| **v == AskVerdict::Wrong).count();
        Ok((judged.len(), wrong))
    }

    /// Held decisions of `model` that leaned since Decide's line last moved, judged or not: what a
    /// lower line would have called settled.
    pub async fn leaning_record(&self, model: &str) -> Result<usize> {
        Ok(self
            .decides_since_move(model)
            .await?
            .iter()
            .filter(|r| r.held && !r.weak && r.standing.as_deref() == Some("leaning"))
            .count())
    }

    /// Every judged decision of `model` read under the wording of `bears` in force, for Insights.
    pub async fn decide_shares(&self, model: &str) -> Result<DecideShares> {
        let mut out = DecideShares::default();
        for DecideRow {
            held,
            weak,
            standing,
            verdict,
        } in self.decides_since(model, 0).await?
        {
            let Some(v) = verdict else { continue };
            let count = |(n, hit): &mut (usize, usize), yes: bool| {
                *n += 1;
                *hit += usize::from(yes);
            };
            if weak {
                count(&mut out.weak, v == AskVerdict::Right);
            }
            if held {
                count(&mut out.held, v == AskVerdict::NothingHere);
                if standing.as_deref() == Some("split") {
                    count(&mut out.split, v == AskVerdict::Right);
                }
            } else {
                count(&mut out.not_held, v == AskVerdict::Wrong);
            }
        }
        Ok(out)
    }

    async fn decides_since_move(&self, model: &str) -> Result<Vec<DecideRow>> {
        let era = crate::core::decide::settled_era(model);
        let since = self
            .moved_at(crate::core::decide::SETTLED_ID, &era, false)
            .await?;
        self.decides_since(model, since).await
    }

    /// Weak decisions a person called right, oldest first, whose vector is under the live embed model.
    pub async fn weak_right_decisions(&self, embed_model: &str, limit: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM decide_events
                WHERE weak = 1 AND verdict = 'right' AND embed_model = ? AND vec_dim > 0
                ORDER BY created_at ASC, id ASC LIMIT ?"#,
            embed_model,
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Unjudged, not weak, not `not_held`, under this decider and BEARS_VERSION, oldest first.
    pub async fn decisions_unread(
        &self,
        decider: &str,
        embed_model: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM decide_events
                WHERE verdict IS NULL AND weak = 0 AND not_held = 0
                  AND decider = ? AND COALESCE(bears_version, 1) = ?
                  AND embed_model = ? AND vec_dim > 0
                ORDER BY created_at ASC, id ASC LIMIT ?"#,
            decider,
            crate::core::decide::state::BEARS_VERSION,
            embed_model,
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// `model`'s decisions made after `since` in Decide's era: one read under another wording of
    /// `bears` is not comparable.
    async fn decides_since(&self, model: &str, since: i64) -> Result<Vec<DecideRow>> {
        let rows = sqlx::query!(
            r#"SELECT questions, standing, not_held AS "not_held!: i64", weak AS "weak!: i64", verdict FROM decide_events
                WHERE decider = ? AND created_at > ? AND COALESCE(bears_version, 1) = ?"#,
            model,
            since,
            crate::core::decide::state::BEARS_VERSION
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                // A row this binary cannot read has no standing.
                let first = crate::core::decide::wire::stored_questions(&r.questions)
                    .ok()
                    .and_then(|qs| qs.into_iter().next());
                let standing = first.and_then(|q| {
                    serde_json::from_str::<serde_json::Value>(&r.standing)
                        .ok()?
                        .get(&q.key)?
                        .as_str()
                        .map(str::to_string)
                });
                DecideRow {
                    held: r.not_held == 0,
                    weak: r.weak != 0,
                    standing,
                    verdict: r.verdict.as_deref().and_then(AskVerdict::parse),
                }
            })
            .collect())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::store::Store;

    /// A judged decision made under the configured `decider` on one noul, `verdict`, whose standing
    /// is `standing`; the server named itself otherwise.
    pub(crate) async fn judged(
        store: &Store,
        decider: &str,
        standing: &str,
        not_held: bool,
        verdict: AskVerdict,
    ) -> String {
        judged_weak(store, decider, standing, not_held, false, verdict).await
    }

    /// `judged`, that weighed the closest notes under the line when `weak`.
    pub(crate) async fn judged_weak(
        store: &Store,
        decider: &str,
        standing: &str,
        not_held: bool,
        weak: bool,
        verdict: AskVerdict,
    ) -> String {
        let id = unjudged(store, decider, standing, not_held, weak, &[]).await;
        store.judge_decision(&id, verdict, None).await.unwrap();
        id
    }

    /// `judged_weak` before any verdict, resting jointly on `joint`.
    pub(crate) async fn unjudged(
        store: &Store,
        decider: &str,
        standing: &str,
        not_held: bool,
        weak: bool,
        joint: &[&str],
    ) -> String {
        let q = crate::infer::decide::Question::noul("verdict", "Keep it?");
        store
            .record_decision(NewDecision {
                question: "Keep it?".into(),
                questions: crate::core::decide::wire::questions_json(std::slice::from_ref(&q)),
                situation: None,
                scope: None,
                filters: "{}".into(),
                answers: (!not_held).then(|| r#"{"verdict":{"yes":0.9}}"#.into()),
                prior: "{}".into(),
                standing: serde_json::json!({ "verdict": standing }).to_string(),
                not_held,
                weak,
                model: "as-reported".into(),
                decider: decider.into(),
                read: 0,
                dropped: 0,
                query_vec: vec![1.0],
                embed_model: "fake".into(),
                evidence: joint
                    .iter()
                    .enumerate()
                    .map(|(rank, a)| NewEvidence {
                        rank,
                        artifact_id: (*a).into(),
                        bears: 0.9,
                        bearing: true,
                        joint: true,
                        n: Some(rank + 1),
                        read: r#"{"verdict":"yes"}"#.into(),
                    })
                    .collect(),
            })
            .await
            .unwrap()
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
            weak: false,
            model: "clef".into(),
            decider: "clef".into(),
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

    /// Four unjudged decisions a deep look read again, one it did not: (store, the first's id).
    async fn read_again() -> (Store, String) {
        use super::test_support::unjudged;
        use crate::store::deep_looks::{DeepKind, DeepSource};
        use crate::store::gaps::GapKind;
        let s = Store::memory().await.unwrap();
        let version = crate::core::judging::catalogue::answers_query("").version;
        let mut first = None;
        for (weak, yes) in [(false, 0.2), (false, 0.9), (true, 0.9), (true, 0.2)] {
            let id = unjudged(&s, "clef", "settled", false, weak, &["a"]).await;
            let kind = match weak {
                false => DeepKind::SecondReading,
                true => DeepKind::Gap(GapKind::Decide),
            };
            let src = DeepSource {
                kind,
                id: id.clone(),
            };
            s.put_deep_look(&src, &[("a".into(), yes)], version, "clef")
                .await
                .unwrap();
            first.get_or_insert(id);
        }
        unjudged(&s, "clef", "settled", false, false, &["a"]).await;
        (s, first.unwrap())
    }

    #[tokio::test]
    async fn a_second_reading_stands_in_for_a_verdict_while_the_labels_count() {
        let (s, _) = read_again().await;
        let e = s.bears_evidence("clef", 0.7, 0.5, true).await.unwrap();
        let want = BearsEvidence {
            too_low: 1,
            too_high: 1,
            fine: 2,
        };
        assert_eq!(e, want);
        let e = s.bears_evidence("clef", 0.7, 0.5, false).await.unwrap();
        assert_eq!(e, BearsEvidence::default());
    }

    #[tokio::test]
    async fn a_verdict_replaces_the_second_reading_of_its_decision() {
        let (s, first) = read_again().await;
        s.judge_decision(&first, AskVerdict::Right, None)
            .await
            .unwrap();
        let e = s.bears_evidence("clef", 0.7, 0.5, true).await.unwrap();
        let want = BearsEvidence {
            too_low: 0,
            too_high: 1,
            fine: 3,
        };
        assert_eq!(e, want);
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
            sqlx::query!(
                "UPDATE decide_events SET created_at = ? WHERE id = ?",
                back,
                id
            )
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
    async fn decisions_read_under_another_wording_of_bears_are_in_no_share_and_no_record() {
        use super::test_support::judged;
        use crate::core::decide::state::BEARS_VERSION;
        let s = Store::memory().await.unwrap();
        judged(&s, "clef", "split", false, AskVerdict::Right).await;
        let before = judged(&s, "clef", "split", false, AskVerdict::Right).await;
        let other = judged(&s, "clef", "settled", false, AskVerdict::Wrong).await;
        // A row from before the version was kept was read under the first wording.
        sqlx::query!(
            "UPDATE decide_events SET bears_version = NULL WHERE id = ?",
            before
        )
        .execute(&s.pool)
        .await
        .unwrap();
        let next = i64::from(BEARS_VERSION) + 1;
        sqlx::query!(
            "UPDATE decide_events SET bears_version = ? WHERE id = ?",
            next,
            other
        )
        .execute(&s.pool)
        .await
        .unwrap();
        let counted = 1 + usize::from(BEARS_VERSION == 1);
        let d = s.decide_shares("clef").await.unwrap();
        assert_eq!(d.split, (counted, counted));
        assert_eq!(d.held, (counted, 0), "the other wording's is not held here");
        assert_eq!(s.settled_record("clef").await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn purging_takes_the_evidence_with_it() {
        let s = Store::memory().await.unwrap();
        s.record_decision(sample()).await.unwrap();
        assert_eq!(s.purge_decisions().await.unwrap(), 1);
        let left: i64 = sqlx::query_scalar!("SELECT count(*) FROM decide_evidence")
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

    #[tokio::test]
    async fn a_held_decision_observes_its_joint_evidence_as_cited() {
        use crate::store::generations::{GenerationParams, NewGeneration};
        use crate::store::observations::Source;
        let s = Store::memory().await.unwrap();
        let generation = s
            .record_generation(&NewGeneration {
                params: GenerationParams::default(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        let mut d = sample();
        // Bearing but packed out: weighed alone, not in the verdict.
        d.evidence[2].joint = false;
        s.record_decision(d).await.unwrap();
        let obs = s
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs.len(), 1, "only what the joint read weighed");
        assert_eq!(obs[0].artifact_id.as_deref(), Some("art-0"));
        assert_eq!(obs[0].rank, Some(1), "its place in the evidence");
        assert_eq!(obs[0].source, Source::Cited);
        assert_eq!(obs[0].query, "Keep bge-m3?");

        let mut n = sample();
        n.not_held = true;
        n.answers = None;
        s.record_decision(n).await.unwrap();
        assert_eq!(
            s.observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a decision the base did not hold cites nothing"
        );
    }

    #[tokio::test]
    async fn a_row_written_without_weak_reads_as_not_weak() {
        let s = Store::memory().await.unwrap();
        // The column list from before `weak` was kept.
        sqlx::query!(
            "INSERT INTO decide_events
               (id, question, questions, prior, standing, not_held, model, read, dropped,
                query_vec, vec_dim, embed_model, created_at)
             VALUES ('old', 'Keep it?', '{}', '{}', '{}', 0, 'clef', 0, 0, x'', 0, 'fake', 0)"
        )
        .execute(&s.pool)
        .await
        .unwrap();
        assert!(!s.decision("old").await.unwrap().unwrap().weak);
    }
}
