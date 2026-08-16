//! The consolidation review queue.
//!
//! Pairs similar enough to be worth attention but not similar enough to
//! supersede without asking. The sweep finds the same pair on every run, so a
//! row here is also the record that a decision was already made about it — a
//! dismissed pair must stay dismissed, or dismissing would achieve nothing.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;

/// How many unreadable replies a pair is worth before the judge stops being
/// offered it.
///
/// One unit's full retry lifetime, which is already the most varied asking the
/// prompt can do: `judge_prompt` carries the attempt number precisely so those
/// retries are not the same question replayed out of the endpoint's cache. If
/// five differently-phrased asks about the same two artifacts all come back
/// unreadable, a sixth is not a better bet than the one after it, and the
/// close-out in `run_one` hands the pair to the next sweep — which without this
/// ceiling would arm it for five more, every sweep, forever.
///
/// The pair stays `pending`, so nothing is lost: it is still on the review
/// queue, and an operator settles it by hand.
pub const MAX_UNREADABLE_JUDGEMENTS: i64 = super::jobs::MAX_ATTEMPTS;

/// How many pending pairs `open_component` may follow outward from its seed.
///
/// A bound on the walk, not on which pairs may be judged: the seed is read
/// directly and is always in the component, however far down the score ordering
/// it sits. Small under `cfg(test)` so that the difference between the two is
/// something a test can actually reach.
#[cfg(not(test))]
const COMPONENT_WINDOW: i64 = 5_000;
#[cfg(test)]
const COMPONENT_WINDOW: i64 = 3;

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
    /// Scored at or above `auto_supersede`. Settled by the sweep's free
    /// clustering pass, and never armed for a model call: that band is answered
    /// correctly by a rule that costs nothing, and spending a call there is the
    /// free path quietly becoming a paid one.
    ///
    /// Filed rather than acted on where it is found, because resolving pairs one
    /// at a time leaves A pointing at a B that is itself hidden — which is what
    /// the sweep's union-find exists to prevent.
    NearIdentical,
    /// The component this pair belongs to draws on more captured roots than
    /// `merge_max_roots`. Not merged, and surfaced instead: a merge of forty
    /// sources is no longer one atomic piece of knowledge, which is what an
    /// artifact is defined to be.
    Oversized,
}

impl PairState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairState::Pending => "pending",
            PairState::NoConflict => "no_conflict",
            PairState::Contradiction => "contradiction",
            PairState::Superseded => "superseded",
            PairState::Dismissed => "dismissed",
            PairState::NearIdentical => "near_identical",
            PairState::Oversized => "oversized",
        }
    }
    pub fn parse(s: &str) -> PairState {
        match s {
            "no_conflict" => PairState::NoConflict,
            "contradiction" => PairState::Contradiction,
            "superseded" => PairState::Superseded,
            "dismissed" => PairState::Dismissed,
            "near_identical" => PairState::NearIdentical,
            "oversized" => PairState::Oversized,
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
    /// How many of those calls came back with something that could not be
    /// parsed as a verdict. The ceiling that stops asking is on this and not on
    /// `judge_attempts` — see `MAX_UNREADABLE_JUDGEMENTS`.
    pub judge_unreadable: i64,
    /// Which artifact the judge named obsolete, when `state` is `Superseded`.
    /// Lets the review UI offer "apply supersede" without asking the model
    /// again.
    pub obsolete_id: Option<String>,
    /// Which merged artifact answered this pair, when the settlement was an
    /// applied merge. What the stranded-merge reap uses to reopen exactly the
    /// pairs a merge that never embedded had closed.
    pub merged_into: Option<String>,
}

pub(crate) fn row_to_pair(r: &sqlx::sqlite::SqliteRow) -> ArtifactPair {
    ArtifactPair {
        id: r.get("id"),
        a_id: r.get("a_id"),
        b_id: r.get("b_id"),
        score: r.get::<f64, _>("score") as f32,
        state: PairState::parse(r.get::<String, _>("state").as_str()),
        detail: r.get("detail"),
        created_at: r.get("created_at"),
        judge_attempts: r.get("judge_attempts"),
        judge_unreadable: r.get("judge_unreadable"),
        obsolete_id: r.get("obsolete_id"),
        merged_into: r.get("merged_into"),
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

    /// File a pair for review, saying where it came from.
    ///
    /// `record_pair` with a `detail`, for the one producer that is not the
    /// similarity sweep: a link the judge found to be a disguised duplicate has
    /// no cosine behind it, so its `score` is genuinely zero and the detail is
    /// what explains the row on a page that otherwise renders a percentage.
    ///
    /// `INSERT OR IGNORE`, like `record_pair`: a pair an operator already
    /// dismissed must not be re-filed by a link.
    pub async fn record_pair_with_detail(
        &self,
        a: &str,
        b: &str,
        score: f32,
        detail: &str,
    ) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, detail, created_at)
             VALUES (?, ?, ?, 'pending', ?, ?)",
        )
        .bind(a)
        .bind(b)
        .bind(score as f64)
        .bind(detail)
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

    /// Put a pair back to `pending` after the action its settlement recorded
    /// turned out not to have happened.
    ///
    /// For a producer that files the decision before carrying it out, which is
    /// the only ordering that cannot leave an artifact hidden with no row
    /// explaining it. The cost of that ordering is a settled row over an action
    /// that then failed, and this is how that is paid back: the pair reopens,
    /// and the next unit re-derives it.
    ///
    /// `from` narrows the write to a row still in the state the caller wrote,
    /// so a decision that landed in between — a judge's, an operator's — is
    /// never reopened underneath them.
    pub async fn unsettle_pair(&self, a: &str, b: &str, from: PairState) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query(
            "UPDATE artifact_pairs SET state = 'pending'
              WHERE a_id = ? AND b_id = ? AND state = ?",
        )
        .bind(a)
        .bind(b)
        .bind(from.as_str())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// The state of the pair between two artifacts, whichever way round they
    /// were filed, or `None` if it was never filed.
    ///
    /// For a producer that is about to act on a pair without asking anyone:
    /// a row that is no longer `pending` carries a decision — the judge's, a
    /// person's, or an earlier automatic hide that a person then undid — and a
    /// local rule that re-derives the same answer must not overrule it.
    pub async fn pair_state_between(&self, a: &str, b: &str) -> Result<Option<PairState>> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM artifact_pairs WHERE a_id = ? AND b_id = ?")
                .bind(a)
                .bind(b)
                .fetch_optional(&self.pool)
                .await?;
        Ok(state.as_deref().map(PairState::parse))
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
    /// to apply it. `merged_into` is cleared for the same reason: it belongs to
    /// the merged settlement `set_pair_merged` writes, and to nothing else.
    pub async fn set_pair_state(
        &self,
        id: i64,
        state: PairState,
        detail: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = ?, detail = ?, obsolete_id = NULL, merged_into = NULL
              WHERE id = ?",
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
            "UPDATE artifact_pairs
                SET state = 'superseded', detail = ?, obsolete_id = ?, merged_into = NULL
              WHERE id = ?",
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

    /// Settle a pair as answered by an applied merge. `merged_into` names the
    /// merged artifact, which is what lets the stranded-merge reap reopen
    /// exactly the pairs a merge that never embedded had closed.
    pub async fn set_pair_merged(
        &self,
        id: i64,
        merged_into: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'no_conflict', detail = ?, merged_into = ?, obsolete_id = NULL
              WHERE id = ?",
        )
        .bind(detail)
        .bind(merged_into)
        .bind(id)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// Reopen every pair a now-dead merge had settled, handing them to a
    /// person. Contradiction rather than Pending on purpose: re-arming the
    /// model would regenerate the same unembeddable draft, at full price,
    /// forever.
    pub async fn reopen_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'contradiction', detail = ?, merged_into = NULL
              WHERE merged_into = ?",
        )
        .bind(detail)
        .bind(merged_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Dismiss every pair a merge being undone had settled, by the lineage the
    /// settlement recorded. `pairs_among` covers only what the merge had
    /// already hidden — before the embed lands that is nothing, while the
    /// pairs were settled the moment the merge was written. Dismissed, not
    /// Contradiction: an undo is an operator overruling the verdict, and
    /// `record_pair` respecting dismissed rows is what makes that last.
    pub async fn dismiss_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'dismissed', detail = ?, merged_into = NULL
              WHERE merged_into = ?",
        )
        .bind(detail)
        .bind(merged_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Pending pairs in the order the judge should spend its budget on them.
    ///
    /// Least-attempted first, then by score. A pair whose reply could not be
    /// parsed stays pending on purpose, and under a plain `score DESC` the same
    /// top-scoring handful would absorb every sweep's budget forever while the
    /// rest of the queue is never reached.
    ///
    /// Pairs past `MAX_UNREADABLE_JUDGEMENTS` are held back. Staying pending is
    /// what keeps them on an operator's review queue; being handed to the judge
    /// again is what would make them cost model calls forever, since a unit that
    /// exhausts its retries is closed rather than parked and the next sweep
    /// would find the pair pending, idle, and first in line all over again.
    pub async fn pairs_to_judge(&self, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query(
            "SELECT * FROM artifact_pairs
              WHERE state = 'pending' AND judge_unreadable < ?
              ORDER BY judge_attempts ASC, score DESC, created_at DESC LIMIT ?",
        )
        .bind(MAX_UNREADABLE_JUDGEMENTS)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_pair).collect())
    }

    /// Every `Pending` pair reachable from this one through a shared artifact.
    ///
    /// One component, one call. Three related artifacts settled pairwise cost
    /// two calls and produce a merged artifact that is superseded almost
    /// immediately — and with the re-merge rule flattening to captured roots
    /// each time, the intermediate merge is written and thrown away for nothing.
    ///
    /// Computed at the moment of use rather than snapshotted when the unit was
    /// armed. Membership changes while a unit waits out a backoff, and acting on
    /// a stale group would rewrite artifacts that have since been answered.
    ///
    /// `Pending` only. A dismissed, near-identical or oversized row carries a
    /// decision, and following it would pull an already-settled artifact into a
    /// group that is about to be superseded.
    ///
    /// The window bounds how far a component may grow, and nothing else. It is
    /// ordered by score and capped, so on a base with more than `WINDOW` pending
    /// pairs the lowest-scoring ones fall outside it — and reading the seed from
    /// the window alone returned an empty component for exactly those. The unit
    /// then found fewer than two members, settled an empty slice, and returned
    /// without recording an attempt; `pairs_to_judge` orders by `judge_attempts`
    /// ascending, so that pair sorted to the front and was armed again on every
    /// tick, consuming a slot of the per-tick budget forever without ever being
    /// judged. The seed is read directly and joins the window if it is missing.
    pub async fn open_component(&self, pair_id: i64) -> Result<Vec<ArtifactPair>> {
        let mut open = self
            .pairs_by_state(PairState::Pending, COMPONENT_WINDOW)
            .await?;
        if !open.iter().any(|p| p.id == pair_id) {
            let seed = self.get_pair(pair_id).await?;
            // Settled while the unit waited. That is the one case where an empty
            // component is the right answer, and `run` treats it as such.
            if seed.state != PairState::Pending {
                return Ok(vec![]);
            }
            open.push(seed);
        }
        let seed = open
            .iter()
            .find(|p| p.id == pair_id)
            .expect("the block above put the seed in the window or returned");

        // Adjacency once, then a flood fill from the seed's two artifacts. A
        // pair joins the component only once one of its artifacts is already
        // in it, and the pair that brings that artifact in can come anywhere
        // in the list — the fixed-point loop this replaces got that right by
        // rescanning the whole window per growth pass, which is quadratic at
        // the window size for one long chain.
        let mut by_artifact: std::collections::HashMap<&str, Vec<usize>> = Default::default();
        for (i, p) in open.iter().enumerate() {
            by_artifact.entry(p.a_id.as_str()).or_default().push(i);
            by_artifact.entry(p.b_id.as_str()).or_default().push(i);
        }
        let mut picked: std::collections::HashSet<i64> = [seed.id].into_iter().collect();
        let mut queue: Vec<&str> = vec![seed.a_id.as_str(), seed.b_id.as_str()];
        let mut seen: std::collections::HashSet<&str> = queue.iter().copied().collect();
        while let Some(id) = queue.pop() {
            for &i in by_artifact.get(id).into_iter().flatten() {
                let p = &open[i];
                picked.insert(p.id);
                for other in [p.a_id.as_str(), p.b_id.as_str()] {
                    if seen.insert(other) {
                        queue.push(other);
                    }
                }
            }
        }
        Ok(open
            .into_iter()
            .filter(|p| picked.contains(&p.id))
            .collect())
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

    /// Count one reply that came back unreadable. Only this counter gates
    /// whether the pair is ever asked about again, so it is stepped where the
    /// parse fails and nowhere else — an endpoint that is down must not shelve
    /// the whole review queue on its way past.
    pub async fn record_unreadable_judgement(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE artifact_pairs SET judge_unreadable = judge_unreadable + 1 WHERE id = ?",
        )
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
    async fn a_pair_the_judge_can_never_read_stops_costing_calls() {
        // The unit closes itself at `MAX_ATTEMPTS` so a later sweep can decide
        // again whether the pair is worth asking about. Nothing implemented that
        // decision: the pair was still pending with no live job, so every sweep
        // armed it for another five calls — and the attempt counter in the
        // prompt means all five are full-cost generations rather than replays
        // out of the endpoint's cache. This is the decision.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_to_judge(10).await.unwrap()[0].id;

        for _ in 0..MAX_UNREADABLE_JUDGEMENTS - 1 {
            s.record_judge_attempt(id).await.unwrap();
            s.record_unreadable_judgement(id).await.unwrap();
        }
        assert_eq!(
            s.pairs_to_judge(10).await.unwrap().len(),
            1,
            "a pair short of the ceiling was already being held back"
        );

        s.record_judge_attempt(id).await.unwrap();
        s.record_unreadable_judgement(id).await.unwrap();
        assert!(
            s.pairs_to_judge(10).await.unwrap().is_empty(),
            "a pair the judge has never once been able to read is still being asked about"
        );
        // Held back from the judge, not settled: it is still on the queue an
        // operator works through by hand.
        assert_eq!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "holding a pair back from the judge took it off the review queue"
        );
    }

    #[tokio::test]
    async fn an_endpoint_outage_does_not_shelve_the_review_queue() {
        // Attempts are counted before the call, so a run of failures against a
        // dead endpoint walks `judge_attempts` to the ceiling for every pending
        // pair at once. Gating on that counter would mean one outage emptied the
        // judge's queue permanently, which is why the ceiling is on replies that
        // came back and could not be read.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_to_judge(10).await.unwrap()[0].id;

        for _ in 0..MAX_UNREADABLE_JUDGEMENTS * 3 {
            s.record_judge_attempt(id).await.unwrap();
        }
        assert_eq!(
            s.pairs_to_judge(10).await.unwrap().len(),
            1,
            "an endpoint outage put the pair permanently out of the judge's reach"
        );
    }

    #[tokio::test]
    async fn a_near_identical_pair_is_never_offered_to_the_model() {
        // The >= auto_supersede band is settled for free by clustering. A pair
        // filed there that reached the dedupe queue would spend a model call on
        // exactly the case where the cheap rule is already correct — the free
        // path quietly becoming a paid one, which is the expensive regression
        // and the silent one.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_settled_pair(&a, &b, 0.99, PairState::NearIdentical)
            .await
            .unwrap();

        assert!(s.pairs_to_judge(10).await.unwrap().is_empty());
        assert_eq!(
            s.pairs_by_state(PairState::NearIdentical, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the pair has to stay findable: the cluster pass reads these rows"
        );
    }

    #[tokio::test]
    async fn an_oversized_pair_leaves_the_pending_queue_but_stays_visible() {
        // Past the fan-in cap nothing is merged, and the pair must not sit on
        // the pending queue costing a call per sweep for a decision that will
        // always come out the same way. It still has to be listed, or the
        // component becomes invisible rather than deferred.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(p, PairState::Oversized, Some("9 roots, cap is 8"))
            .await
            .unwrap();

        assert!(s.pairs_to_judge(10).await.unwrap().is_empty());
        let found = s.pairs_by_state(PairState::Oversized, 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("9 roots, cap is 8"));
    }

    #[tokio::test]
    async fn every_state_survives_a_round_trip_through_the_database() {
        // `parse` falls back to Pending for anything it does not know, so a
        // state whose string is missing from either half reads back as an
        // unanswered pair — and the sweep would ask about it again forever.
        for state in [
            PairState::Pending,
            PairState::NoConflict,
            PairState::Contradiction,
            PairState::Superseded,
            PairState::Dismissed,
            PairState::NearIdentical,
            PairState::Oversized,
        ] {
            let s = Store::memory().await.unwrap();
            let (a, b) = two_artifacts(&s).await;
            s.record_pair(&a, &b, 0.91).await.unwrap();
            let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
            if state != PairState::Pending {
                s.set_pair_state(p, state, None).await.unwrap();
            }
            assert_eq!(
                s.get_pair(p).await.unwrap().state,
                state,
                "{} did not survive the round trip",
                state.as_str()
            );
        }
    }

    /// Four artifacts under one corpus, for the component tests.
    async fn four_artifacts(s: &Store) -> Vec<String> {
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
        s.insert_artifacts(&src.id, &new)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect()
    }

    #[tokio::test]
    async fn a_component_gathers_every_pending_pair_that_shares_an_artifact() {
        // One component, one call. Merging a four-artifact group pairwise costs
        // three calls and writes two merged artifacts that are superseded
        // almost immediately.
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        s.record_pair(&m[1], &m[2], 0.90).await.unwrap();
        s.record_pair(&m[3], &m[0], 0.89).await.unwrap();
        let seed = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        let comp = s.open_component(seed).await.unwrap();
        assert_eq!(comp.len(), 3, "the component stopped short: {comp:?}");
    }

    #[tokio::test]
    async fn a_component_is_the_same_set_whichever_pair_seeds_it() {
        // The fixed point is what makes this true. A single pass would return a
        // component whose contents depended on which row happened to come
        // first, so two units for one group could act on different sets.
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        s.record_pair(&m[1], &m[2], 0.90).await.unwrap();
        s.record_pair(&m[2], &m[3], 0.89).await.unwrap();

        let all = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        let mut sets: Vec<Vec<i64>> = Vec::new();
        for p in &all {
            let mut ids: Vec<i64> = s
                .open_component(p.id)
                .await
                .unwrap()
                .into_iter()
                .map(|c| c.id)
                .collect();
            ids.sort_unstable();
            sets.push(ids);
        }
        assert_eq!(sets[0].len(), 3);
        assert!(
            sets.iter().all(|s| *s == sets[0]),
            "the component depended on which pair seeded it: {sets:?}"
        );
    }

    #[tokio::test]
    async fn a_settled_pair_never_drags_an_answered_artifact_into_a_component() {
        // Pending only. A dismissed or near-identical row carries a decision,
        // and following it would pull an already-settled artifact into a group
        // that is about to be superseded and rewritten.
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        s.record_pair(&m[1], &m[2], 0.90).await.unwrap();
        let all = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        let (seed, other) = (all[0].id, all[1].id);
        s.set_pair_state(other, PairState::Dismissed, None)
            .await
            .unwrap();

        assert_eq!(s.open_component(seed).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pair_below_the_window_is_still_the_seed_of_its_own_component() {
        // The window bounds how far the walk may go, not which pairs may be
        // judged. It is ordered by score, so on a base with more pending pairs
        // than it holds the lowest-scoring ones fall outside — and reading the
        // seed from the window alone gave those an empty component. The unit
        // then found fewer than two members, settled an empty slice and returned
        // without recording an attempt, and `pairs_to_judge` orders by attempts
        // ascending: the pair sorted to the front and was armed again every
        // tick, spending a slot of the per-tick budget forever without ever
        // being judged.
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.99).await.unwrap();
        s.record_pair(&m[1], &m[2], 0.98).await.unwrap();
        s.record_pair(&m[2], &m[3], 0.97).await.unwrap();
        s.record_pair(&m[3], &m[0], 0.90).await.unwrap();

        let window = s
            .pairs_by_state(PairState::Pending, COMPONENT_WINDOW)
            .await
            .unwrap();
        let all = s.pairs_by_state(PairState::Pending, 100).await.unwrap();
        let seed = all
            .iter()
            .find(|p| !window.iter().any(|w| w.id == p.id))
            .expect("the fixture must put one pair outside the window")
            .id;

        let comp = s.open_component(seed).await.unwrap();

        assert!(
            comp.iter().any(|p| p.id == seed),
            "a pair outside the window has no component of its own: {comp:?}"
        );
    }

    #[tokio::test]
    async fn a_component_of_a_pair_that_is_no_longer_open_is_empty() {
        // The unit re-reads its own pair before acting. A sibling unit that
        // already settled the group must find nothing to do rather than an
        // empty-but-plausible component.
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(p, PairState::NoConflict, None)
            .await
            .unwrap();

        assert!(s.open_component(p).await.unwrap().is_empty());
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

    #[tokio::test]
    async fn a_settlement_whose_action_failed_is_reopened() {
        // The producer files the decision before carrying it out, because the
        // other order can leave an artifact hidden with no row explaining it.
        // This is the payment for that order: the action failed, so the row
        // saying it happened has to go back, or the pair is settled over two
        // artifacts that are both still visible and nothing looks again.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_settled_pair(&a, &b, 0.99, PairState::NoConflict)
            .await
            .unwrap();

        assert!(
            s.unsettle_pair(&b, &a, PairState::NoConflict)
                .await
                .unwrap(),
            "the pair did not reopen, and filed either way round is the same pair"
        );
        assert_eq!(
            s.pair_state_between(&a, &b).await.unwrap(),
            Some(PairState::Pending)
        );
    }

    #[tokio::test]
    async fn reopening_leaves_a_decision_that_landed_in_between_alone() {
        // The narrowing that makes the reopen safe. Between the settlement and
        // the failure, a judge or an operator can settle the same pair their
        // own way, and undoing that is the one thing this must never do.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_settled_pair(&a, &b, 0.99, PairState::NoConflict)
            .await
            .unwrap();
        let id = s.pairs_by_state(PairState::NoConflict, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Dismissed, None)
            .await
            .unwrap();

        assert!(
            !s.unsettle_pair(&a, &b, PairState::NoConflict)
                .await
                .unwrap(),
            "a decision made in between was reopened underneath it"
        );
        assert_eq!(
            s.pair_state_between(&a, &b).await.unwrap(),
            Some(PairState::Dismissed)
        );
    }

    #[tokio::test]
    async fn a_merged_settlement_records_which_merge_answered_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        s.set_pair_merged(id, "merge-1", Some("same claim"))
            .await
            .unwrap();

        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.merged_into.as_deref(), Some("merge-1"));

        // Leaving the settlement drops the record, exactly as obsolete_id does.
        s.set_pair_state(id, PairState::Dismissed, None)
            .await
            .unwrap();
        assert_eq!(s.get_pair(id).await.unwrap().merged_into, None);
    }

    #[tokio::test]
    async fn reopening_a_stranded_merge_s_pairs_touches_only_its_own() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_merged(id, "merge-1", None).await.unwrap();

        assert_eq!(
            s.reopen_pairs_merged_into("merge-other", "unrelated")
                .await
                .unwrap(),
            0,
            "another merge's undo reopened this pair"
        );
        assert_eq!(
            s.reopen_pairs_merged_into("merge-1", "the merged text could not be indexed")
                .await
                .unwrap(),
            1
        );
        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::Contradiction);
        assert_eq!(p.merged_into, None);
    }
}
