//! The consolidation review queue.
//!
//! Pairs similar enough to be worth attention but not similar enough to
//! supersede without asking. The sweep finds the same pair on every run, so a
//! row here is also the record that a decision was already made about it — a
//! dismissed pair must stay dismissed, or dismissing would achieve nothing.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    /// Found by the sweep, nothing has looked at it yet.
    Pending,
    /// The fact-token prefilter or the judge found nothing to disagree about.
    NoConflict,
    /// The judge found a detail the two artifacts state differently, with no
    /// clear direction — both readings could still be current.
    Contradiction,
    /// The judge named which artifact is obsolete (`obsolete_id`) with enough
    /// confidence to propose a supersede, but it is not applied automatically:
    /// an operator confirms via the pair's "apply supersede" action.
    Superseded,
    /// An operator looked and decided there is nothing here.
    Dismissed,
}

impl PairState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairState::Pending => "pending",
            PairState::NoConflict => "no_conflict",
            PairState::Contradiction => "contradiction",
            PairState::Superseded => "superseded",
            PairState::Dismissed => "dismissed",
        }
    }
    pub fn parse(s: &str) -> PairState {
        match s {
            "no_conflict" => PairState::NoConflict,
            "contradiction" => PairState::Contradiction,
            "superseded" => PairState::Superseded,
            "dismissed" => PairState::Dismissed,
            _ => PairState::Pending,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArtifactPair {
    pub id: i64,
    pub a_id: String,
    pub b_id: String,
    pub score: f32,
    pub state: PairState,
    pub detail: Option<String>,
    pub created_at: i64,
    /// Model calls this pair has already cost, successful or not. Orders the
    /// judge's queue so a pair it cannot read does not starve the rest.
    pub judge_attempts: i64,
    /// Which artifact the judge named obsolete, when `state` is `Superseded`.
    /// Lets the review UI offer "apply supersede" without asking the model
    /// again.
    pub obsolete_id: Option<String>,
}

fn row_to_pair(r: &sqlx::sqlite::SqliteRow) -> ArtifactPair {
    ArtifactPair {
        id: r.get("id"),
        a_id: r.get("a_id"),
        b_id: r.get("b_id"),
        score: r.get::<f64, _>("score") as f32,
        state: PairState::parse(r.get::<String, _>("state").as_str()),
        detail: r.get("detail"),
        created_at: r.get("created_at"),
        judge_attempts: r.get("judge_attempts"),
        obsolete_id: r.get("obsolete_id"),
    }
}

impl Store {
    /// File a pair for review. Returns whether this was new.
    ///
    /// `INSERT OR IGNORE` rather than an upsert, deliberately: the sweep finds
    /// the same pair every run, and re-arming a row an operator dismissed
    /// would make dismissing pointless.
    pub async fn record_pair(&self, a: &str, b: &str, score: f32) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
             VALUES (?, ?, ?, 'pending', ?)",
        )
        .bind(a)
        .bind(b)
        .bind(score as f64)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// File a pair that is already answered. Returns whether this changed
    /// anything.
    ///
    /// A row that carries a real decision — a person's dismissal, the judge's
    /// contradiction — is left alone: the sweep re-finds the same pair every
    /// run, and overwriting the answer with its own opinion would make
    /// dismissing pointless. `pending` is not a decision, it is the absence of
    /// one, so it is answered here. That is also what settles pairs filed
    /// before the sweep could answer them, without a migration.
    pub async fn record_settled_pair(
        &self,
        a: &str,
        b: &str,
        score: f32,
        state: PairState,
    ) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "INSERT INTO artifact_pairs (a_id, b_id, score, state, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(a_id, b_id) DO UPDATE SET state = excluded.state
               WHERE artifact_pairs.state = 'pending'",
        )
        .bind(a)
        .bind(b)
        .bind(score as f64)
        .bind(state.as_str())
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn get_pair(&self, id: i64) -> Result<ArtifactPair> {
        let row = sqlx::query("SELECT * FROM artifact_pairs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(crate::error::Error::NotFound)?;
        Ok(row_to_pair(&row))
    }

    pub async fn pairs_by_state(&self, state: PairState, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query(
            "SELECT * FROM artifact_pairs WHERE state = ?
              ORDER BY score DESC, created_at DESC LIMIT ?",
        )
        .bind(state.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_pair).collect())
    }

    /// How many pairs sit in a state, for a page that shows only the first few
    /// of them and has to say how many it is not showing.
    pub async fn count_pairs_by_state(&self, state: PairState) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM artifact_pairs WHERE state = ?")
                .bind(state.as_str())
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Move a pair to any state other than `Superseded`, clearing the judge's
    /// proposed direction along with it.
    ///
    /// `obsolete_id` is cleared rather than left alone because it belongs to the
    /// `Superseded` state and to nothing else — see `set_pair_superseded`, the
    /// only path that writes it. A pair the judge proposed a winner for and an
    /// operator then dismissed would otherwise keep naming a supersede that was
    /// explicitly rejected, and any later listing of dismissed pairs would offer
    /// to apply it.
    pub async fn set_pair_state(
        &self,
        id: i64,
        state: PairState,
        detail: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE artifact_pairs SET state = ?, detail = ?, obsolete_id = NULL WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(detail)
        .bind(id)
        .execute(&self.pool)
        .await?;
        // A dismiss from a stale Ops page names a pair that is no longer there
        // — its artifacts were deleted, or the row never existed. Redirecting
        // as though it worked tells the operator the queue is one shorter than
        // it is.
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// Record the judge's proposed direction: `state` becomes `Superseded`,
    /// `obsolete_id` names which artifact it believes is stale. Separate from
    /// `set_pair_state` because this is the only path that writes
    /// `obsolete_id`, and a plain contradiction must never carry a stale value
    /// left over from a different pair.
    pub async fn set_pair_superseded(
        &self,
        id: i64,
        obsolete_id: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE artifact_pairs SET state = 'superseded', detail = ?, obsolete_id = ? WHERE id = ?",
        )
        .bind(detail)
        .bind(obsolete_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// Pending pairs in the order the judge should spend its budget on them.
    ///
    /// Least-attempted first, then by score. A pair whose reply could not be
    /// parsed stays pending on purpose, and under a plain `score DESC` the same
    /// top-scoring handful would absorb every sweep's budget forever while the
    /// rest of the queue is never reached.
    pub async fn pairs_to_judge(&self, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query(
            "SELECT * FROM artifact_pairs WHERE state = 'pending'
              ORDER BY judge_attempts ASC, score DESC, created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_pair).collect())
    }

    /// Count one model call against a pair, whether or not it produced an
    /// answer. Written before the call, so a run that dies mid-judgement still
    /// leaves the pair at the back of the next sweep's queue.
    pub async fn record_judge_attempt(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE artifact_pairs SET judge_attempts = judge_attempts + 1 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifacts::NewArtifact;

    async fn two_artifacts(s: &Store) -> (String, String) {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &src.id,
                &[
                    NewArtifact {
                        ordinal: 0,
                        text: "one".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                    NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: None,
                        caveats: vec![],
                    },
                ],
            )
            .await
            .unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[tokio::test]
    async fn a_pair_is_recorded_once_and_only_once() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        assert!(s.record_pair(&a, &b, 0.91).await.unwrap());
        assert!(
            !s.record_pair(&a, &b, 0.91).await.unwrap(),
            "a repeat sweep duplicated the pair"
        );
        assert!(
            !s.record_pair(&b, &a, 0.91).await.unwrap(),
            "the reversed pair duplicated it"
        );
        assert_eq!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn resolving_a_pair_takes_it_off_the_pending_list() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);

        s.set_pair_state(
            p.id,
            PairState::Contradiction,
            Some("version differs: 1.2 vs 1.4"),
        )
        .await
        .unwrap();

        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let done = s
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0].detail.as_deref(),
            Some("version differs: 1.2 vs 1.4")
        );
    }

    #[tokio::test]
    async fn a_resolved_pair_is_not_re_queued_by_the_next_sweep() {
        // The sweep re-finds the same pair every run. If `record_pair` reset a
        // dismissed row to pending, dismissing would achieve nothing.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_state(p.id, PairState::Dismissed, None)
            .await
            .unwrap();

        assert!(!s.record_pair(&a, &b, 0.91).await.unwrap());
        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn leaving_the_superseded_state_drops_the_judge_s_proposal() {
        // `obsolete_id` belongs to `Superseded` and to no other state. A pair
        // the judge proposed a winner for and an operator then dismissed used to
        // keep naming that winner, so any listing of dismissed pairs would offer
        // to apply a supersede that had been explicitly rejected.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_superseded(p.id, &a, Some("a is stale"))
            .await
            .unwrap();
        assert_eq!(
            s.get_pair(p.id).await.unwrap().obsolete_id.as_deref(),
            Some(a.as_str())
        );

        s.set_pair_state(p.id, PairState::Dismissed, None)
            .await
            .unwrap();

        assert_eq!(
            s.get_pair(p.id).await.unwrap().obsolete_id,
            None,
            "a dismissed pair still carries the supersede that was rejected"
        );
    }

    #[tokio::test]
    async fn settling_answers_a_pending_pair_and_respects_a_real_decision() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;

        // Pending is the absence of a decision, so the sweep may answer it.
        s.record_pair(&a, &b, 0.91).await.unwrap();
        assert!(
            s.record_settled_pair(&a, &b, 0.91, PairState::NoConflict)
                .await
                .unwrap()
        );
        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );

        // A dismissal is a decision. The sweep re-finds this pair every run and
        // must not talk over it.
        let p = s
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_state(p.id, PairState::Dismissed, None)
            .await
            .unwrap();
        s.record_settled_pair(&a, &b, 0.91, PairState::NoConflict)
            .await
            .unwrap();
        assert_eq!(
            s.pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a decision was overwritten"
        );
    }

    #[tokio::test]
    async fn dismissing_a_pair_that_is_no_longer_there_is_an_error() {
        // Ops pages go stale, and a pair whose artifacts were deleted is gone
        // with them. Reporting success would tell the operator the queue is one
        // shorter than it is.
        let s = Store::memory().await.unwrap();
        assert!(matches!(
            s.set_pair_state(9999, PairState::Dismissed, None).await,
            Err(crate::error::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_judge_queue_puts_the_least_attempted_pair_first() {
        let s = Store::memory().await.unwrap();
        let src = s.insert_corpus("four", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..4)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        let (a, b, c, d) = (
            made[0].id.clone(),
            made[1].id.clone(),
            made[2].id.clone(),
            made[3].id.clone(),
        );
        // The higher score would otherwise always lead.
        s.record_pair(&a, &b, 0.99).await.unwrap();
        s.record_pair(&c, &d, 0.90).await.unwrap();
        let first = s.pairs_to_judge(10).await.unwrap();
        assert_eq!(first[0].score, 0.99);

        s.record_judge_attempt(first[0].id).await.unwrap();
        let next = s.pairs_to_judge(10).await.unwrap();
        assert_eq!(
            next[0].score, 0.90,
            "a pair that already cost a call must not lead again"
        );
        assert_eq!(next[1].judge_attempts, 1);
    }

    #[tokio::test]
    async fn deleting_an_artifact_takes_its_pairs_with_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        s.delete_artifact(&a).await.unwrap();
        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
