//! The consolidation review queue.
//!
//! Pairs similar enough to be worth attention but not similar enough to
//! supersede without asking. The sweep finds the same pair on every run, so a
//! row here is also the record that a decision was already made about it — a
//! dismissed pair must stay dismissed, or dismissing would achieve nothing.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;
use std::collections::{HashMap, HashSet};

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
    /// Scored at or above `auto_supersede`, and settled by the sweep's free
    /// clustering pass rather than by a model call.
    ///
    /// Nothing files this any more — a pair in that band now goes to the judge
    /// like every other, first in line. The variant and the pass that closes it
    /// stay for the rows an older base filed.
    ///
    /// Filed rather than acted on where it is found, because resolving pairs one
    /// at a time leaves A pointing at a B that is itself hidden — which is what
    /// the sweep's union-find exists to prevent.
    NearIdentical,
    /// Deprecated and never written. The variant survives one release so that
    /// rows already carrying it still parse.
    ///
    /// It meant the component this pair belonged to drew on more captured roots
    /// than `merge_max_roots` — terminal, and reached before any call was made,
    /// so nothing could ever return to it. The unit judges two artifacts at a
    /// time now, which removes the condition, and `reopen_oversized` puts the
    /// rows left behind back in the queue.
    Oversized,
    /// The judge found that neither artifact states anything — two containers
    /// rather than two claims.
    ///
    /// Nothing files this any more. It was a recommendation an operator
    /// confirmed, which left the one answer that clears a pair of empty
    /// artifacts waiting on a person holding no more evidence than the judge
    /// had; `jobs::dedupe::discard_both` now retires both sides where the
    /// verdict is found and settles the pair `Dismissed`, as an applied
    /// replacement does. Deprecation is reversible, so the undo is the review.
    ///
    /// The variant and everything that renders it stay for the rows an older
    /// base filed, which are still waiting on that press.
    Vacuous,
    /// A lifecycle event took one of the two artifacts out of results, so the
    /// question cannot be acted on — not because anyone answered it.
    ///
    /// Every button the review queue carries ends in `Core::supersede`,
    /// `deprecate` or a merge, and all three refuse a side that is not active.
    /// A row naming one is therefore off the queue whatever it says, and it
    /// needs a state that says so: `Dismissed` is an operator's decision and
    /// must stay binding forever (`record_pair` is `INSERT OR IGNORE`), which
    /// is exactly the property that would make a lifecycle event silently
    /// permanent.
    ///
    /// Terminal only while the artifact is away. `Core::reactivate` and
    /// `unsupersede` put these rows back as `Pending`
    /// (`reopen_stale_pairs`), which is what makes both ways out of results
    /// reversible in the queue as well as in the artifact list. The verdict
    /// itself does not come back: it was pronounced about a pair one of whose
    /// sides has since been away, and re-asking costs one call against
    /// carrying a ruling nobody re-checked.
    ///
    /// Same shape as `Oversized` — settled without an answer, reopened by a
    /// later pass — and unlike it, still written.
    Stale,
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
            PairState::Vacuous => "vacuous",
            PairState::Stale => "stale",
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
            "vacuous" => PairState::Vacuous,
            "stale" => PairState::Stale,
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

    /// The pairs in a state that a person can still act on: both artifacts are
    /// in results.
    ///
    /// The review queue's buttons all end in `Core::supersede`, `deprecate` or
    /// a merge, and every one of those refuses a side that is not active. A row
    /// naming an artifact that has since been hidden or deprecated is therefore
    /// a question with no answer available — offering it produced
    /// `cannot supersede: loser … is superseded` on the press, which is a
    /// correct guard reporting a queue that should never have listed the row.
    ///
    /// Read-side and not the whole story, because both ways out of results are
    /// reversible: an operator restores the artifact and the pair is a real
    /// question again. What a lifecycle event does to the row itself is
    /// elsewhere — `follow_supersession` moves a supersession's rows onto the
    /// winner, and `stale_unreachable_pairs` settles what is left `Stale`,
    /// reversibly, so that a verdict nobody can act on is not merely invisible.
    /// This filter is what keeps the queue honest in between: the row is off it
    /// from the moment the artifact leaves results, without waiting for a
    /// sweep.
    pub async fn pairs_awaiting_review(
        &self,
        state: PairState,
        limit: i64,
    ) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query(
            "SELECT p.* FROM artifact_pairs p
               JOIN artifacts a ON a.id = p.a_id
               JOIN artifacts b ON b.id = p.b_id
              WHERE p.state = ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
              ORDER BY p.score DESC, p.created_at DESC LIMIT ?",
        )
        .bind(state.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_pair).collect())
    }

    /// How many pairs `pairs_awaiting_review` would return without a limit.
    ///
    /// Counted under the same rule as the listing, so the "N more waiting" line
    /// under a queue that shows the first few cannot promise rows the queue
    /// will never render.
    pub async fn count_pairs_awaiting_review(&self, state: PairState) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifact_pairs p
               JOIN artifacts a ON a.id = p.a_id
               JOIN artifacts b ON b.id = p.b_id
              WHERE p.state = ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL",
        )
        .bind(state.as_str())
        .fetch_one(&self.pool)
        .await?)
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

    /// Move every still-open pair that names one of `old` onto `new_id`.
    ///
    /// Called when a merge is finished, so that a duplicate of one of its
    /// sources becomes a duplicate of the merge rather than dying with the
    /// source. Without this a cluster only converges by waiting for the merge
    /// to embed and a later similarity sweep to re-file the same question,
    /// which is a whole tick per generation.
    ///
    /// Three rows never move. One whose other side is already `new_id` would
    /// become a pair of the merge with itself. One that would collide with an
    /// existing pair between the same two artifacts must leave that row alone,
    /// whatever state it is in — that is what keeps an operator's dismissal
    /// binding, the same property `record_pair`'s `INSERT OR IGNORE` provides.
    /// Both are dismissed instead, because the merge has answered the question
    /// they carried.
    ///
    /// `Pending` only, and by this point that is a statement about ordering
    /// rather than a rule of its own. `merge::finish` hides each root through
    /// `Core::supersede`, which runs `follow_supersession` first, so a root's
    /// verdicts have already moved onto the merge under that function's rules
    /// by the time this runs — and what it finds here is what those rules left
    /// as `Pending`. The two agree: a supersession takes the loser's open
    /// questions with it, verdicts included, and the carry-over is marked as an
    /// inference on the row.
    ///
    /// `judge_attempts` and `judge_unreadable` reset, because the moved row
    /// asks about a different pair of artifacts than the one that earned those
    /// counts. This cannot loop: every merge takes an artifact out of results,
    /// so the sequence of merges a cluster can produce is finite.
    ///
    /// `score` is deliberately left alone and is now stale — it was measured
    /// between the old member and the other side. It orders the judge queue and
    /// gates nothing by this point, so the staleness costs ordering accuracy
    /// and nothing else.
    pub async fn repoint_open_pairs(&self, old: &[String], new_id: &str) -> Result<u64> {
        let mut moved = 0u64;
        for o in old {
            if o == new_id {
                continue;
            }
            let rows = sqlx::query(
                "SELECT * FROM artifact_pairs
                  WHERE state = 'pending' AND (a_id = ? OR b_id = ?)",
            )
            .bind(o)
            .bind(o)
            .fetch_all(&self.pool)
            .await?;
            for p in rows.iter().map(row_to_pair) {
                let other = if p.a_id == *o {
                    p.b_id.clone()
                } else {
                    p.a_id.clone()
                };
                // Both sides went into this merge, or the other side is the
                // merge already. Either way the row would name the merge twice.
                // Tested against `old` and not only against `new_id`, because
                // the sides are visited one at a time: a pair between two
                // sources is moved onto the merge while the first is being
                // handled and only recognised as a self-pair while the second
                // is, which leaves it counted as moved.
                if other == new_id || old.contains(&other) {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("both of these went into the same merge"),
                    )
                    .await?;
                    continue;
                }
                if self.pair_state_between(&other, new_id).await?.is_some() {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("a pair between these two already exists"),
                    )
                    .await?;
                    continue;
                }
                let (a, b) = if other.as_str() <= new_id {
                    (other.as_str(), new_id)
                } else {
                    (new_id, other.as_str())
                };
                sqlx::query(
                    "UPDATE artifact_pairs
                        SET a_id = ?, b_id = ?, judge_attempts = 0, judge_unreadable = 0,
                            detail = NULL
                      WHERE id = ?",
                )
                .bind(a)
                .bind(b)
                .bind(p.id)
                .execute(&self.pool)
                .await?;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// The pair between two artifacts, whichever way round they were filed.
    ///
    /// `pair_state_between` answers the question a producer asks — "has anyone
    /// decided about these two" — and is the right shape for it. This is for
    /// the one caller that has to act on the row itself.
    pub async fn pair_between(&self, a: &str, b: &str) -> Result<Option<ArtifactPair>> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let row = sqlx::query("SELECT * FROM artifact_pairs WHERE a_id = ? AND b_id = ?")
            .bind(a)
            .bind(b)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(row_to_pair))
    }

    /// Put a pair back in the judge queue, whatever it was carrying.
    ///
    /// Counters and detail reset for the reason `repoint_open_pairs` resets
    /// them: the row is being asked again from nothing, and attempts earned
    /// under an older question would push it straight past the ceilings that
    /// decide whether it is worth a call.
    async fn reopen_pair(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL, obsolete_id = NULL, merged_into = NULL,
                    judge_attempts = 0, judge_unreadable = 0
              WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move every question still open about `loser` onto the artifact that
    /// replaced it, and settle the ones the supersession itself put out of
    /// reach.
    ///
    /// The merge path has done this since merges existed
    /// (`repoint_open_pairs`); a supersession is the same event — an artifact
    /// leaves results and another one stands for it — and did not. What that
    /// left behind is a pair naming an artifact that is no longer in results,
    /// still listed on the review queue, whose "keep this one" press
    /// `Core::supersede` refuses: the loser it would hide already points at a
    /// winner, and superseding it again would build the chain `A -> B -> C`
    /// that nothing in the UI can follow.
    ///
    /// Four rows settle `Stale` instead of moving. One whose other side *is*
    /// the winner would become a pair of the winner with itself, and the
    /// supersession has answered what it asked. One whose `obsolete_id` names
    /// the loser proposed hiding the artifact that is now hidden, so the
    /// proposal is spent. One that would collide with an existing pair between
    /// the same two artifacts leaves that row alone — the rule
    /// `repoint_open_pairs` keeps for the same reason, and what makes an
    /// operator's dismissal binding. And a `Vacuous` verdict, which is a
    /// statement about the two bodies it was pronounced over: the judge is
    /// required to find that *neither* states anything
    /// (`infer::prompt::DEDUPE_SYSTEM`), so carrying it onto the winner would
    /// assert emptiness of an artifact no judge ever read — and the press it
    /// arms retires both sides (`jobs::dedupe::discard_both`), which would take
    /// out the artifact the sweep just chose as its cluster's keeper.
    ///
    /// `Stale` and not `Dismissed` for all four: none of them is an answer, and
    /// a dismissal is binding forever. Undoing the supersession puts them back
    /// (`reopen_stale_pairs`).
    ///
    /// A collision where the moving row carries a verdict reopens the surviving
    /// row, when what that row carries is a machine verdict that has since been
    /// contradicted — `NoConflict` or `NearIdentical`. This is the common case
    /// and not the rare one: the sweep files pairs by cosine, so if X is near
    /// the loser and the winner is near enough the loser to stand for it, an
    /// `(X, winner)` row usually already exists. Without this, `(L, X)`
    /// "contradiction: timeout 30s vs 90s" settles against a surviving row that
    /// says there is nothing to look at, and an operator-actionable
    /// disagreement leaves every queue with nothing recording that it existed.
    /// A survivor that carries a verdict of its own is left alone — two
    /// verdicts is not a reason to spend a call — and so is a `Dismissed` one,
    /// because a person decided that and this module's whole premise is that
    /// the decision holds.
    ///
    /// A verdict otherwise moves with its state rather than reopening as
    /// `Pending`. The judge found that two artifacts disagree; the winner of a
    /// supersession was near enough its loser to stand for it, so the
    /// disagreement with the other side is very probably still there, and
    /// re-filing it as pending would spend a call to re-learn it. That
    /// carry-over is an inference and not a fresh ruling, which is what the
    /// appended detail says — a person reading the row has to know the verdict
    /// was pronounced over a different artifact. A `Pending` row carries no
    /// verdict, so it moves the way the merge path moves it: counters reset,
    /// detail cleared, because the question it now asks is about a different
    /// pair of artifacts.
    ///
    /// Only rows that are still waiting on someone. `NoConflict`, `Dismissed`
    /// and `Oversized` are answered questions, and `NearIdentical` is the
    /// sweep's own filing, which recomputes liveness for the whole cluster
    /// before it acts (`jobs::consolidate`).
    ///
    /// `score` is left alone and is now stale, exactly as in
    /// `repoint_open_pairs`: it orders the judge queue and gates nothing here.
    pub async fn follow_supersession(&self, loser: &str, winner: &str) -> Result<u64> {
        if loser == winner {
            return Ok(0);
        }
        let rows = sqlx::query(
            "SELECT * FROM artifact_pairs
              WHERE state IN ('pending', 'contradiction', 'superseded', 'vacuous')
                AND (a_id = ? OR b_id = ?)",
        )
        .bind(loser)
        .bind(loser)
        .fetch_all(&self.pool)
        .await?;

        let mut moved = 0u64;
        for p in rows.iter().map(row_to_pair) {
            let other = if p.a_id == loser {
                p.b_id.clone()
            } else {
                p.a_id.clone()
            };
            if other == winner {
                self.set_pair_state(
                    p.id,
                    PairState::Stale,
                    Some("the supersession of these two answered this"),
                )
                .await?;
                continue;
            }
            if p.obsolete_id.as_deref() == Some(loser) {
                self.set_pair_state(
                    p.id,
                    PairState::Stale,
                    Some("the artifact this proposed hiding is already hidden"),
                )
                .await?;
                continue;
            }
            if p.state == PairState::Vacuous {
                self.set_pair_state(
                    p.id,
                    PairState::Stale,
                    Some(
                        "a vacuous verdict is about the two bodies it was read over, \
                          and one of them is hidden",
                    ),
                )
                .await?;
                continue;
            }
            if let Some(existing) = self.pair_between(&other, winner).await? {
                // The verdict this row carries has nowhere to land, and the row
                // standing in its place says the opposite. Ask again rather
                // than let the disagreement disappear.
                if p.state != PairState::Pending
                    && matches!(
                        existing.state,
                        PairState::NoConflict | PairState::NearIdentical
                    )
                {
                    self.reopen_pair(existing.id).await?;
                }
                self.set_pair_state(
                    p.id,
                    PairState::Stale,
                    Some("a pair between these two already exists"),
                )
                .await?;
                continue;
            }
            let (a, b) = if other.as_str() <= winner {
                (other.as_str(), winner)
            } else {
                (winner, other.as_str())
            };
            if p.state == PairState::Pending {
                sqlx::query(
                    "UPDATE artifact_pairs
                        SET a_id = ?, b_id = ?, judge_attempts = 0, judge_unreadable = 0,
                            detail = NULL
                      WHERE id = ?",
                )
                .bind(a)
                .bind(b)
                .bind(p.id)
                .execute(&self.pool)
                .await?;
            } else {
                // Prose and no ids: this lands in `PairRow.detail`, which
                // Capture renders verbatim beside two artifacts it has already
                // named by title, and a raw ULID there is the one string on
                // that surface a reader cannot resolve.
                let note = "carried over from the artifact this one superseded";
                let detail = match p.detail.as_deref() {
                    Some(d) => format!("{d} ({note})"),
                    None => note.to_string(),
                };
                sqlx::query(
                    "UPDATE artifact_pairs SET a_id = ?, b_id = ?, detail = ? WHERE id = ?",
                )
                .bind(a)
                .bind(b)
                .bind(detail)
                .bind(p.id)
                .execute(&self.pool)
                .await?;
            }
            moved += 1;
        }
        Ok(moved)
    }

    /// Settle `Stale` every open pair naming an artifact that is no longer in
    /// results, and say how many.
    ///
    /// The other half of the read-side rule in `pairs_awaiting_review`. Such a
    /// row is off every queue and out of every count, which means the Dismiss
    /// button that would have settled it is unreachable too: it accumulated
    /// silently, invisible to the operator and to the "N more waiting" line
    /// alike, for as long as nobody thought to restore the artifact.
    ///
    /// Mostly the deprecated half — an operator retiring one side of a pair the
    /// judge had already ruled on — because a supersession's rows are moved
    /// onto the winner instead. Not only that half: what
    /// `follow_supersession` could not move settles here too, which is what
    /// stops a supersession chain whose end is itself out of results from
    /// stranding rows nothing lists and nothing repairs.
    ///
    /// Run after the sweep's repair pass, never before it: moving a question
    /// onto the artifact that answers it beats taking it off the queue.
    ///
    /// And bounded to what that pass has finished with, which is the whole
    /// reason this is three queries rather than one UPDATE. `follow_supersessions`
    /// takes 200 supersessions a sweep so a backlog drains over a few ticks; an
    /// unbounded settle running straight afterwards reached the other 200-and-up
    /// first and marked them `Stale`. That is a one-way door:
    /// `supersessions_with_open_pairs` selects only
    /// `('pending','contradiction','superseded','vacuous')`, so a stale row is
    /// invisible to the repair for ever, and `reopen_stale_pairs` needs both
    /// sides back in results, which a superseded loser never is. The verdict
    /// was never carried onto the winner and no later tick could carry it —
    /// biting hardest on exactly the upgrade and adoption cases the repair pass
    /// exists for. So a pair whose out-of-results side has a supersession chain
    /// ending somewhere still in results is left alone: it is a question the
    /// repair still owes a move, and it will get one within a few ticks.
    ///
    /// `Stale` rather than `Dismissed` so that restoring the artifact brings
    /// the question back (`reopen_stale_pairs`). Both ways out of results are
    /// reversible; settling these as somebody's decision would make the queue
    /// the one place where they are not.
    pub async fn stale_unreachable_pairs(&self) -> Result<u64> {
        let rows = sqlx::query(
            "SELECT p.id AS pair_id, x.id AS side, x.superseded_by AS winner
               FROM artifact_pairs p
               JOIN artifacts x ON x.id IN (p.a_id, p.b_id)
              WHERE p.state IN ('pending', 'contradiction', 'superseded', 'vacuous')
                AND (x.status <> 'active' OR x.superseded_by IS NOT NULL)",
        )
        .fetch_all(&self.pool)
        .await?;

        // One walk per superseded artifact, not per pair that names it: a
        // cluster's loser is usually on several open pairs at once.
        let mut chain_end: HashMap<String, Option<String>> = HashMap::new();
        let mut owed: HashSet<i64> = HashSet::new();
        let mut candidates: HashSet<i64> = HashSet::new();
        for r in &rows {
            let pair_id: i64 = r.get("pair_id");
            candidates.insert(pair_id);
            let side: String = r.get("side");
            let Some(winner): Option<String> = r.get("winner") else {
                continue;
            };
            let end = match chain_end.get(&winner) {
                Some(e) => e.clone(),
                None => {
                    let e = self.end_of_supersession_chain(&winner).await?;
                    chain_end.insert(winner.clone(), e.clone());
                    e
                }
            };
            if end.is_some_and(|w| w != side) {
                owed.insert(pair_id);
            }
        }
        let to_settle: Vec<i64> = candidates
            .into_iter()
            .filter(|id| !owed.contains(id))
            .collect();
        if to_settle.is_empty() {
            return Ok(0);
        }

        let mut settled = 0u64;
        // Chunked well under SQLITE_MAX_VARIABLE_NUMBER, which is 999 on the
        // builds that predate 3.32 and is not worth finding out about at
        // runtime on a repair pass.
        for chunk in to_settle.chunks(500) {
            let holes = vec!["?"; chunk.len()].join(", ");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE artifact_pairs
                    SET state = 'stale', obsolete_id = NULL, merged_into = NULL,
                        detail = 'one of these artifacts is no longer in results'
                  WHERE id IN ({holes})"
            )));
            for id in chunk {
                q = q.bind(id);
            }
            settled += q.execute(&self.pool).await?.rows_affected();
        }
        Ok(settled)
    }

    /// Put back the questions an artifact's departure from results had taken
    /// off the queue, now that it is back.
    ///
    /// `Core::reactivate` and `unsupersede` restore the artifact itself; they
    /// left the review queue behind. Without this, undoing a supersession or a
    /// deprecation restores an artifact whose open contradictions are gone for
    /// good — settled `Stale` by `follow_supersession` or by
    /// `stale_unreachable_pairs`, and re-filing is blocked by `record_pair`'s
    /// `INSERT OR IGNORE`, so the sweep will never ask again either.
    ///
    /// `Pending` and not the state the row carried. The rows this reopens were
    /// settled for reasons that are now void, not answered, and the ones a
    /// supersession settled were about a pairing that no longer exists — asking
    /// again costs one call and is the only answer that is actually current.
    ///
    /// Only where the *other* side is in results too, so restoring one artifact
    /// does not put a question back that names another artifact somebody
    /// retired in the meantime. That row stays `Stale` and comes back with its
    /// own artifact.
    pub async fn reopen_stale_pairs(&self, artifact_id: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL, obsolete_id = NULL, merged_into = NULL,
                    judge_attempts = 0, judge_unreadable = 0
              WHERE state = 'stale'
                AND (a_id = ?1 OR b_id = ?1)
                AND NOT EXISTS (
                      SELECT 1 FROM artifacts x
                       WHERE x.id IN (artifact_pairs.a_id, artifact_pairs.b_id)
                         AND (x.status <> 'active' OR x.superseded_by IS NOT NULL))",
        )
        .bind(artifact_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Every supersession that still has a question open about the artifact it
    /// hid, as `(loser, winner)`.
    ///
    /// What the sweep's repair pass reads. `Core::supersede` moves those pairs
    /// where the supersession happens, and cannot be the only place that does:
    /// a crash between the row write and the move, the callers that write
    /// `set_superseded_by` directly, and every row filed before that rule
    /// existed all leave the same drift behind.
    ///
    /// The winner reported is the end of the chain, not the artifact the loser
    /// names. `A -> B -> C` is a state the merge path can reach —
    /// `repoint_supersession` exists to avoid it and warns rather than fails
    /// when it cannot — and reading only the first hop meant A's open pairs
    /// were re-pointed onto a B that is itself out of results, or, with a
    /// liveness test on that first hop, never repaired at all. Nothing else
    /// repairs a chain, so those rows were invisible on every queue and absent
    /// from every count with no pass that would ever reach them.
    ///
    /// A chain that ends somewhere that is not in results yields nothing: there
    /// is no artifact to move the question to. Those rows settle `Stale`
    /// instead (`stale_unreachable_pairs`), which is what keeps them findable
    /// again if the end of the chain comes back.
    pub async fn supersessions_with_open_pairs(&self, limit: i64) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT DISTINCT loser.id AS loser, loser.superseded_by AS winner
               FROM artifacts loser
               JOIN artifact_pairs p ON p.a_id = loser.id OR p.b_id = loser.id
              WHERE loser.superseded_by IS NOT NULL
                AND p.state IN ('pending', 'contradiction', 'superseded', 'vacuous')
              LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::new();
        for r in rows {
            let loser: String = r.get("loser");
            let first: String = r.get("winner");
            if let Some(winner) = self.end_of_supersession_chain(&first).await?
                && winner != loser
            {
                out.push((loser, winner));
            }
        }
        Ok(out)
    }

    /// Follow `superseded_by` from `from` to the artifact a reader would
    /// actually land on, or `None` if that is not an artifact in results.
    ///
    /// Bounded and cycle-guarded rather than trusting the data. Nothing writes
    /// a cycle deliberately, and a repair that hangs the sweep on one row would
    /// be a worse failure than the drift it is here to fix.
    async fn end_of_supersession_chain(&self, from: &str) -> Result<Option<String>> {
        const MAX_HOPS: usize = 32;
        let mut at = from.to_string();
        let mut seen = vec![at.clone()];
        for _ in 0..MAX_HOPS {
            let row = sqlx::query("SELECT status, superseded_by FROM artifacts WHERE id = ?")
                .bind(&at)
                .fetch_optional(&self.pool)
                .await?;
            let Some(row) = row else { return Ok(None) };
            match row.get::<Option<String>, _>("superseded_by") {
                Some(next) => {
                    if seen.contains(&next) {
                        tracing::warn!(artifact = %at, "a supersession chain loops");
                        return Ok(None);
                    }
                    seen.push(next.clone());
                    at = next;
                }
                None => {
                    return Ok((row.get::<String, _>("status") == "active").then_some(at));
                }
            }
        }
        tracing::warn!(artifact = %from, "a supersession chain is longer than the repair follows");
        Ok(None)
    }

    /// Put every pair the old fan-in cap refused back into the judge queue.
    ///
    /// `Oversized` was terminal and reached without a call ever being made: the
    /// component flattened to more roots than the cap and every pair in it was
    /// settled before the model saw anything. Pairwise merging removes the
    /// condition, so the rows left behind are simply unanswered questions.
    ///
    /// Run every sweep rather than once behind a guard. Nothing writes the
    /// state any more, so the first pass drains it and every later one matches
    /// no rows — cheaper than the machinery a one-shot would need.
    ///
    /// `judge_attempts` and `judge_unreadable` reset with the state, for the
    /// same reason `repoint_open_pairs` resets them. "A refused row never spent
    /// a call" holds for most of these and not for all: the sweep recorded an
    /// attempt against every pair of a component, so a pair could take an
    /// unreadable reply, stay `Pending`, and only be settled `Oversized` later
    /// when its component grew past the cap. Reopened with its counts intact,
    /// such a row can come back already at or past `MAX_UNREADABLE_JUDGEMENTS`
    /// — and `pairs_to_judge` holds those back, so it would sit pending for
    /// good, never judged and never surfaced.
    pub async fn reopen_oversized(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL,
                    judge_attempts = 0, judge_unreadable = 0
              WHERE state = 'oversized'",
        )
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

    /// `n` artifacts under one corpus, for the cases that need more than a pair.
    async fn n_artifacts(s: &Store, n: usize) -> Vec<String> {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
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
            .iter()
            .map(|c| c.id.clone())
            .collect()
    }

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

    /// The whole point of re-pointing: C was a duplicate of B, B is now inside
    /// M, so C is a duplicate of M and the question survives the merge.
    #[tokio::test]
    async fn an_open_pair_follows_its_member_into_the_merge() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 4).await;
        let (b, c, m) = (&ids[1], &ids[2], &ids[3]);
        s.record_pair(b, c, 0.91).await.unwrap();

        let moved = s
            .repoint_open_pairs(std::slice::from_ref(b), m)
            .await
            .unwrap();

        assert_eq!(moved, 1);
        assert_eq!(
            s.pair_state_between(c, m).await.unwrap(),
            Some(PairState::Pending),
            "the pair did not follow B into M"
        );
        assert_eq!(
            s.pair_state_between(b, c).await.unwrap(),
            None,
            "the old pair is still there"
        );
    }

    /// A pair between the merge's two own sources becomes a pair of the merge
    /// with itself. There is no question left in it.
    #[tokio::test]
    async fn a_pair_between_two_sources_of_the_same_merge_is_dismissed() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (a, b, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(a, b, 0.91).await.unwrap();

        let moved = s
            .repoint_open_pairs(&[a.clone(), b.clone()], m)
            .await
            .unwrap();

        assert_eq!(moved, 0, "a self-pair was written");
        assert_eq!(
            s.pair_state_between(a, b).await.unwrap(),
            Some(PairState::Dismissed)
        );
    }

    /// An operator's decision outlives the merge. Re-pointing onto a pair
    /// someone already dismissed must not put that question back, which is the
    /// same property `record_pair`'s `INSERT OR IGNORE` provides.
    #[tokio::test]
    async fn re_pointing_onto_an_existing_pair_leaves_the_existing_row_alone() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (b, c, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(c, m, 0.80).await.unwrap();
        let existing = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(existing, PairState::Dismissed, Some("operator"))
            .await
            .unwrap();
        s.record_pair(b, c, 0.91).await.unwrap();

        let moved = s
            .repoint_open_pairs(std::slice::from_ref(b), m)
            .await
            .unwrap();

        assert_eq!(moved, 0);
        assert_eq!(
            s.pair_state_between(c, m).await.unwrap(),
            Some(PairState::Dismissed),
            "an operator's dismissal was overwritten"
        );
        assert_eq!(
            s.pair_state_between(b, c).await.unwrap(),
            Some(PairState::Dismissed)
        );
    }

    /// The re-pointed row asks a different question than the one that earned
    /// the counters, so it must not inherit a backoff from artifacts it no
    /// longer names.
    #[tokio::test]
    async fn a_re_pointed_pair_starts_its_attempts_over() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (b, c, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(b, c, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.record_judge_attempt(id).await.unwrap();
        s.record_unreadable_judgement(id).await.unwrap();

        s.repoint_open_pairs(std::slice::from_ref(b), m)
            .await
            .unwrap();

        let pending = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        let moved = pending
            .iter()
            .find(|p| p.a_id == *m || p.b_id == *m)
            .expect("the row moved");
        assert_eq!(moved.judge_attempts, 0);
        assert_eq!(moved.judge_unreadable, 0);
    }

    /// Only pending rows move. A settled pair is an answered question, and
    /// moving it would re-file someone's verdict against an artifact it was
    /// never about.
    #[tokio::test]
    async fn a_settled_pair_is_not_re_pointed() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (b, c, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(b, c, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::NoConflict, None)
            .await
            .unwrap();

        let moved = s
            .repoint_open_pairs(std::slice::from_ref(b), m)
            .await
            .unwrap();

        assert_eq!(moved, 0);
        assert_eq!(
            s.pair_state_between(b, c).await.unwrap(),
            Some(PairState::NoConflict)
        );
    }

    /// The state was terminal and reached without a call: the component
    /// flattened past the cap and every pair in it was settled before the model
    /// saw anything. Sixteen of these exist in the field.
    #[tokio::test]
    async fn an_oversized_pair_goes_back_into_the_queue() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Oversized, Some("12 sources, cap is 8"))
            .await
            .unwrap();

        assert_eq!(s.reopen_oversized().await.unwrap(), 1);

        let back = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].detail, None,
            "the cap's line is still on a pending row"
        );
        assert_eq!(back[0].judge_attempts, 0);
        // Runs every sweep; once the queue is drained it must do nothing.
        assert_eq!(s.reopen_oversized().await.unwrap(), 0);
    }

    /// "A refused row never spent a call" holds for most of them and not for
    /// all: the sweep recorded an attempt against every pair of a component, so
    /// a pair could take an unreadable reply, stay pending, and be settled
    /// `Oversized` only later when its component grew past the cap. Reopened
    /// with that count intact it comes back at the ceiling — pending, held back
    /// by `pairs_to_judge`, and so never judged and never surfaced again.
    #[tokio::test]
    async fn an_oversized_pair_that_spent_calls_is_judgeable_again() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        for _ in 0..MAX_UNREADABLE_JUDGEMENTS {
            s.record_judge_attempt(id).await.unwrap();
            s.record_unreadable_judgement(id).await.unwrap();
        }
        s.set_pair_state(id, PairState::Oversized, Some("12 sources, cap is 8"))
            .await
            .unwrap();

        assert_eq!(s.reopen_oversized().await.unwrap(), 1);

        let back = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert_eq!(back[0].judge_unreadable, 0);
        assert_eq!(back[0].judge_attempts, 0);
        assert_eq!(
            s.pairs_to_judge(10).await.unwrap().len(),
            1,
            "a reopened pair the judge will never be handed is not reopened"
        );
    }

    /// A pair naming an artifact that has just been superseded is a question
    /// about something no longer in results. The merge path already moves those
    /// onto the artifact that replaced them; a supersession has exactly the same
    /// shape, and left them behind.
    #[tokio::test]
    async fn an_open_pair_follows_its_member_into_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();

        assert_eq!(s.follow_supersession(loser, winner).await.unwrap(), 1);

        let open = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert_eq!(open.len(), 1);
        let mut sides = [open[0].a_id.clone(), open[0].b_id.clone()];
        sides.sort();
        let mut want = [other.clone(), winner.clone()];
        want.sort();
        assert_eq!(sides, want, "the pair still names the artifact that lost");
    }

    /// The two sides of the supersession are the pair. Nothing is left to ask:
    /// re-pointing it would make the winner a pair with itself.
    #[tokio::test]
    async fn a_pair_between_the_two_sides_of_a_supersession_is_settled() {
        let s = Store::memory().await.unwrap();
        let (loser, winner) = two_artifacts(&s).await;
        s.record_pair(&loser, &winner, 0.91).await.unwrap();

        s.follow_supersession(&loser, &winner).await.unwrap();

        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
        // `Stale` and not `Dismissed`: nobody answered this, and undoing the
        // supersession has to bring it back.
        assert_eq!(
            s.pairs_by_state(PairState::Stale, 10).await.unwrap().len(),
            1
        );
    }

    /// A verdict is carried over rather than thrown away or re-asked. The judge
    /// found two artifacts that disagree; the one that survived the supersession
    /// still disagrees with the other side, and re-filing it as pending would
    /// spend a model call to learn what is already known.
    #[tokio::test]
    async fn a_verdict_keeps_its_state_when_it_follows_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Contradiction, Some("30 seconds vs 90"))
            .await
            .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        let open = s
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert!(
            open[0].a_id == *winner || open[0].b_id == *winner,
            "the verdict still names the artifact that lost"
        );
        let detail = open[0].detail.clone().unwrap_or_default();
        assert!(
            detail.contains("30 seconds vs 90"),
            "the judge's reasoning was dropped: {detail}"
        );
        // Prose, not ids: Capture renders this verbatim beside two artifacts it
        // has already named by title.
        assert!(
            detail.contains("carried over"),
            "nothing says the verdict was carried over: {detail}"
        );
        assert!(
            !detail.contains(winner.as_str()) && !detail.contains(loser.as_str()),
            "the note put a raw id in front of a reader: {detail}"
        );
    }

    /// The sweep files pairs by cosine, so a supersession's winner usually
    /// already has a row against the same neighbour the loser did. The moving
    /// verdict cannot land on top of it — and dropping it silently left the
    /// surviving row saying there was nothing to look at, which is the opposite
    /// of what the judge found.
    #[tokio::test]
    async fn a_verdict_with_nowhere_to_land_reopens_the_row_standing_in_its_place() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let verdict = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(verdict, PairState::Contradiction, Some("30 seconds vs 90"))
            .await
            .unwrap();
        // Already judged, and judged the other way.
        s.record_pair(other, winner, 0.72).await.unwrap();
        let standing = s.pair_between(other, winner).await.unwrap().unwrap().id;
        s.set_pair_state(standing, PairState::NoConflict, Some("nothing in common"))
            .await
            .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        assert_eq!(
            s.get_pair(verdict).await.unwrap().state,
            PairState::Stale,
            "the row that could not move was left on the queue"
        );
        let back = s.get_pair(standing).await.unwrap();
        assert_eq!(
            back.state,
            PairState::Pending,
            "the disagreement left every queue with nothing recording it"
        );
        assert_eq!(
            back.judge_attempts, 0,
            "it would come back past its ceiling"
        );
    }

    /// A person decided there was nothing here, and `record_pair`'s
    /// `INSERT OR IGNORE` is built so that decision holds forever. A
    /// supersession is not a reason to ask them again.
    #[tokio::test]
    async fn a_collision_leaves_a_dismissal_alone() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let verdict = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(verdict, PairState::Contradiction, Some("30 seconds vs 90"))
            .await
            .unwrap();
        s.record_pair(other, winner, 0.72).await.unwrap();
        let standing = s.pair_between(other, winner).await.unwrap().unwrap().id;
        s.set_pair_state(standing, PairState::Dismissed, Some("not worth looking at"))
            .await
            .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        assert_eq!(
            s.get_pair(standing).await.unwrap().state,
            PairState::Dismissed,
            "a supersession overruled an operator's dismissal"
        );
    }

    /// `vacuous` is a ruling about the two bodies the judge read: it holds only
    /// if *neither* states anything. Carried onto the winner it would assert
    /// that of an artifact no judge ever saw — and the press it arms retires
    /// both sides, so it would arm a destructive one against the artifact the
    /// sweep had just picked as its cluster's keeper.
    #[tokio::test]
    async fn a_vacuous_verdict_does_not_follow_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(
            id,
            PairState::Vacuous,
            Some("each body is its own file path"),
        )
        .await
        .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::Stale);
        assert!(
            p.a_id != *winner && p.b_id != *winner,
            "a discard nobody judged was armed against the winner"
        );
    }

    /// `A -> B -> C` is a state the merge path can reach, and reading only the
    /// first hop meant A's questions were re-pointed onto an artifact that is
    /// itself out of results — or, with a liveness test on that hop, left where
    /// no queue lists them and no pass repairs them.
    #[tokio::test]
    async fn the_repair_follows_a_supersession_chain_to_its_end() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 4).await;
        let (a, other, b, c) = (&ids[0], &ids[1], &ids[2], &ids[3]);
        s.record_pair(a, other, 0.91).await.unwrap();
        s.set_superseded_by(a, Some(b)).await.unwrap();
        s.set_superseded_by(b, Some(c)).await.unwrap();

        let found = s.supersessions_with_open_pairs(10).await.unwrap();

        assert_eq!(
            found,
            vec![(a.clone(), c.clone())],
            "the repair stopped at the middle of the chain"
        );
    }

    /// The end of the chain is where a reader lands, and if that is out of
    /// results too there is nothing to move the question to. The row is taken
    /// off the queue instead, reversibly.
    #[tokio::test]
    async fn a_chain_that_ends_out_of_results_offers_nothing_to_repair() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (a, other, b) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(a, other, 0.91).await.unwrap();
        s.set_superseded_by(a, Some(b)).await.unwrap();
        s.set_artifact_status(b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        assert!(
            s.supersessions_with_open_pairs(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.stale_unreachable_pairs().await.unwrap(), 1);
    }

    /// The stale pass ran unbounded straight after a repair pass that takes 200
    /// supersessions a sweep, so it reached everything the repair had not got to
    /// yet and settled it `Stale` first. That is a one-way door:
    /// `supersessions_with_open_pairs` does not select stale rows, and
    /// `reopen_stale_pairs` needs both sides back in results, which a superseded
    /// loser never is. The verdict was never carried onto the winner and no
    /// later tick could carry it.
    #[tokio::test]
    async fn a_question_the_repair_still_owes_a_move_is_not_settled_first() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(id, PairState::Contradiction, Some("30 seconds vs 90"))
            .await
            .unwrap();
        s.set_superseded_by(loser, Some(winner)).await.unwrap();

        assert_eq!(
            s.stale_unreachable_pairs().await.unwrap(),
            0,
            "a verdict the repair pass was going to move was settled out from under it"
        );
        assert_eq!(
            s.get_pair(id).await.unwrap().state,
            PairState::Contradiction
        );

        // And the repair, on this tick or a later one, still moves it.
        assert_eq!(s.follow_supersession(loser, winner).await.unwrap(), 1);
        let moved = s.get_pair(id).await.unwrap();
        assert_eq!(moved.state, PairState::Contradiction);
        assert!(
            [&moved.a_id, &moved.b_id].contains(&winner),
            "the verdict did not land on the winner"
        );
    }

    /// The bound is on what the repair can still reach, and nothing else: a
    /// deprecated side has no supersession to follow, so it settles the tick it
    /// is found on, exactly as before.
    #[tokio::test]
    async fn a_deprecated_side_still_comes_off_the_queue_at_once() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        s.set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert_eq!(s.stale_unreachable_pairs().await.unwrap(), 1);
    }

    /// The queue does not show a pair whose member has left results, which also
    /// takes away the Dismiss button that would have settled it. A verdict has
    /// no other drain — `arm_dedupe` only ever settles pending rows — so those
    /// piled up where nobody could see or reach them.
    #[tokio::test]
    async fn a_verdict_nobody_can_act_on_comes_off_the_queue_and_back_again() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pair_between(&a, &b).await.unwrap().unwrap().id;
        s.set_pair_state(id, PairState::Contradiction, Some("30 seconds vs 90"))
            .await
            .unwrap();
        s.set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        assert_eq!(s.stale_unreachable_pairs().await.unwrap(), 1);
        assert_eq!(s.get_pair(id).await.unwrap().state, PairState::Stale);

        s.set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Active)
            .await
            .unwrap();
        assert_eq!(s.reopen_stale_pairs(&b).await.unwrap(), 1);
        assert_eq!(s.get_pair(id).await.unwrap().state, PairState::Pending);
    }

    /// One artifact coming back does not make a question answerable if the
    /// other side is still away — every button on the card would still refuse.
    #[tokio::test]
    async fn a_restore_leaves_a_pair_whose_other_side_is_still_away() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 2).await;
        let (a, b) = (&ids[0], &ids[1]);
        s.record_pair(a, b, 0.91).await.unwrap();
        let id = s.pair_between(a, b).await.unwrap().unwrap().id;
        use crate::store::artifacts::ArtifactStatus;
        s.set_artifact_status(a, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        s.set_artifact_status(b, ArtifactStatus::Deprecated)
            .await
            .unwrap();
        s.stale_unreachable_pairs().await.unwrap();

        s.set_artifact_status(a, ArtifactStatus::Active)
            .await
            .unwrap();
        assert_eq!(s.reopen_stale_pairs(a).await.unwrap(), 0);
        assert_eq!(s.get_pair(id).await.unwrap().state, PairState::Stale);
    }

    /// The judge proposed hiding the very artifact that has since been hidden.
    /// Applying it now would supersede an artifact that already points at a
    /// winner, which `Core::supersede` refuses — so the proposal is spent.
    #[tokio::test]
    async fn a_proposal_to_hide_the_side_that_lost_is_settled() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_superseded(id, loser, Some("the older reading"))
            .await
            .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        assert!(
            s.pairs_by_state(PairState::Superseded, 10)
                .await
                .unwrap()
                .is_empty(),
            "a proposal nobody can apply is still on the queue"
        );
    }

    /// Re-pointing onto a pair that already exists must leave the existing row
    /// alone, whatever state it carries — the same rule the merge path keeps,
    /// and what makes an operator's dismissal binding.
    #[tokio::test]
    async fn following_a_supersession_leaves_an_existing_pair_alone() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        s.record_pair(other, winner, 0.88).await.unwrap();
        let existing = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.a_id != *loser && p.b_id != *loser)
            .expect("the pair between the survivors");
        s.set_pair_state(
            existing.id,
            PairState::NoConflict,
            Some("nothing in common"),
        )
        .await
        .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        let kept = s.get_pair(existing.id).await.unwrap();
        assert_eq!(kept.state, PairState::NoConflict);
        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "the row that could not move is still pending"
        );
    }

    /// A settled pair carries a decision about artifacts it was really about.
    /// Moving it would re-file that decision against an artifact nobody judged.
    #[tokio::test]
    async fn a_settled_pair_is_not_dragged_into_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::NoConflict, Some("nothing in common"))
            .await
            .unwrap();

        assert_eq!(s.follow_supersession(loser, winner).await.unwrap(), 0);

        let kept = s.get_pair(id).await.unwrap();
        assert_eq!(kept.state, PairState::NoConflict);
        assert!(kept.a_id == *loser || kept.b_id == *loser);
    }

    /// The review queue offers a button that supersedes one side of the pair,
    /// and `Core::supersede` refuses a side that is not active. A pair naming
    /// an artifact that has left results is therefore a question nobody can
    /// answer — it must not be listed, and it must not be counted.
    #[tokio::test]
    async fn a_pair_whose_member_left_results_is_not_awaiting_review() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 4).await;
        s.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
        s.record_pair(&ids[2], &ids[3], 0.88).await.unwrap();
        s.set_artifact_status(&ids[1], crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let open = s
            .pairs_awaiting_review(PairState::Pending, 10)
            .await
            .unwrap();
        assert_eq!(open.len(), 1, "a pair nobody can act on is still listed");
        assert_eq!(open[0].a_id, ids[2]);
        assert_eq!(
            s.count_pairs_awaiting_review(PairState::Pending)
                .await
                .unwrap(),
            1
        );
    }

    /// Superseded, not deprecated: the other half of `in_results`, and the one
    /// that produced the error this was found by.
    #[tokio::test]
    async fn a_pair_naming_a_superseded_artifact_is_not_awaiting_review() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        s.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Contradiction, Some("30 vs 90"))
            .await
            .unwrap();
        s.set_superseded_by(&ids[0], Some(&ids[2])).await.unwrap();

        assert!(
            s.pairs_awaiting_review(PairState::Contradiction, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            s.count_pairs_awaiting_review(PairState::Contradiction)
                .await
                .unwrap(),
            0
        );
    }
}
