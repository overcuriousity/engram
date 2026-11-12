//! The consolidation review queue. A row is also the record of a decision: a dismissed pair
//! must stay dismissed, since the sweep re-finds it every run.

use super::{Store, now};
use crate::error::Result;
use sqlx::Row;
use std::collections::{HashMap, HashSet};

/// One unit's full retry lifetime, each ask phrased differently; without the ceiling the
/// sweep re-arms the pair forever. The pair stays `pending`.
pub const MAX_UNREADABLE_JUDGEMENTS: i64 = super::jobs::MAX_ATTEMPTS;

/// Bounds the walk, not which pairs may be judged: the seed is always read directly.
/// Small under `cfg(test)` so a test can reach the difference.
#[cfg(not(test))]
const COMPONENT_WINDOW: i64 = 5_000;
#[cfg(test)]
const COMPONENT_WINDOW: i64 = 3;

/// The open states, in `/consolidation`'s order; only `Pending` and `Contradiction` are
/// still written, the rest are older bases' rows the drain clears.
pub const AWAITING_REVIEW: [PairState; 6] = [
    PairState::Contradiction,
    PairState::Superseded,
    PairState::Duplicate,
    // The drain puts these back to `Pending`; listed until it has run.
    PairState::Vacuous,
    PairState::Unmergeable,
    PairState::Pending,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairState {
    Pending,
    NoConflict,
    /// No longer written: `jobs::dedupe` closes such a pair `NoConflict`. Older rows only.
    Unmergeable,
    /// Left open on purpose: both stay in results, and search and Ask show them together.
    Contradiction,
    /// No longer written: a replacement is applied where found. Older rows only.
    Superseded,
    Dismissed,
    /// No longer filed; the closing pass stays for older rows.
    NearIdentical,
    /// Never written; survives so old rows parse. `reopen_oversized` requeues them.
    Oversized,
    /// No longer filed: `jobs::dedupe::discard_both` acts on the verdict. The drain requeues
    /// older rows.
    Vacuous,
    /// No longer filed: a duplicate verdict writes its merge (loss check, journal and undo make
    /// that safe). Older rows only.
    Duplicate,
    /// Not `Dismissed`, which binds forever: this is undone by `reopen_stale_pairs` when the
    /// artifact returns, and the verdict is re-asked rather than restored.
    Stale,
}

impl PairState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairState::Pending => "pending",
            PairState::NoConflict => "no_conflict",
            PairState::Contradiction => "contradiction",
            PairState::Unmergeable => "unmergeable",
            PairState::Superseded => "superseded",
            PairState::Dismissed => "dismissed",
            PairState::NearIdentical => "near_identical",
            PairState::Oversized => "oversized",
            PairState::Vacuous => "vacuous",
            PairState::Duplicate => "duplicate",
            PairState::Stale => "stale",
        }
    }
    pub fn parse(s: &str) -> PairState {
        match s {
            "no_conflict" => PairState::NoConflict,
            "contradiction" => PairState::Contradiction,
            "unmergeable" => PairState::Unmergeable,
            "superseded" => PairState::Superseded,
            "dismissed" => PairState::Dismissed,
            "near_identical" => PairState::NearIdentical,
            "oversized" => PairState::Oversized,
            "vacuous" => PairState::Vacuous,
            "duplicate" => PairState::Duplicate,
            "stale" => PairState::Stale,
            _ => PairState::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandRecord {
    pub judged: usize,
    pub acted: usize,
    pub undone: usize,
}

/// Not derivable from `state`, and passed rather than defaulted, so a new operator surface
/// cannot be recorded as the model by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    Model,
    Operator,
    /// Never the judge and never a person: an action taken back because the evidence turned.
    Evidence,
}

impl DecidedBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecidedBy::Model => "model",
            DecidedBy::Operator => "operator",
            DecidedBy::Evidence => "evidence",
        }
    }

    /// Anything unrecognised reads as absent: guessing would be the untruth the column ends.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "model" => Some(DecidedBy::Model),
            "operator" => Some(DecidedBy::Operator),
            "evidence" => Some(DecidedBy::Evidence),
            _ => None,
        }
    }
}

/// Read onto search results, so the base never picks a side.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Disagreement {
    pub artifact_id: String,
    /// The same clock as `other_created_at`. Defaulted for older serialized rows.
    #[serde(default)]
    pub created_at: i64,
    pub other_id: String,
    pub other_title: Option<String>,
    pub other_created_at: i64,
    pub detail: Option<String>,
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
    /// `None` on an open pair and on rows from before the column.
    pub decided_by: Option<DecidedBy>,
    /// Orders the judge queue so an unreadable pair does not starve the rest.
    pub judge_attempts: i64,
    /// The ceiling is on this, not `judge_attempts`: see `MAX_UNREADABLE_JUDGEMENTS`.
    pub judge_unreadable: i64,
    /// A column, not a `PairState`: it records a person's decision beside the judge's finding.
    pub synthesis_asked: bool,
    /// Which artifact the judge named obsolete, when `state` is `Superseded`.
    /// Lets the review UI offer "apply supersede" without asking the model
    /// again.
    pub obsolete_id: Option<String>,
    /// Lets the stranded-merge reap reopen exactly the pairs a never-embedded merge closed.
    pub merged_into: Option<String>,
}

/// Two counts: `jobs::did_work` reads this report, and counting only `moved` made the sweep
/// back off with staled work still in front of it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Followed {
    pub moved: u64,
    pub staled: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainCounts {
    pub closed: u64,
    pub rejudged: u64,
}

impl Followed {
    /// For "did anything happen", not for logging what moved.
    pub fn settled(&self) -> u64 {
        self.moved + self.staled
    }
}

#[derive(sqlx::FromRow)]
pub(crate) struct PairRow {
    pub id: i64,
    pub a_id: String,
    pub b_id: String,
    pub score: f64,
    pub state: String,
    pub detail: Option<String>,
    pub created_at: i64,
    pub judge_attempts: i64,
    pub judge_unreadable: i64,
    pub obsolete_id: Option<String>,
    pub merged_into: Option<String>,
    pub decided_by: Option<String>,
    pub synthesis_asked: i64,
}

pub(crate) fn row_to_pair(r: PairRow) -> ArtifactPair {
    ArtifactPair {
        id: r.id,
        a_id: r.a_id,
        b_id: r.b_id,
        score: r.score as f32,
        state: PairState::parse(r.state.as_str()),
        detail: r.detail,
        created_at: r.created_at,
        decided_by: r.decided_by.as_deref().and_then(DecidedBy::parse),
        judge_attempts: r.judge_attempts,
        judge_unreadable: r.judge_unreadable,
        obsolete_id: r.obsolete_id,
        merged_into: r.merged_into,
        synthesis_asked: r.synthesis_asked != 0,
    }
}

impl Store {
    /// `INSERT OR IGNORE`, not an upsert: re-arming a dismissed row defeats dismissing.
    pub async fn record_pair(&self, a: &str, b: &str, score: f32) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query!(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, created_at)
             VALUES (?, ?, ?, 'pending', ?)",
            a,
            b,
            score as f64,
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// A duplicate found via a link has no cosine: `score` is zero and `detail` explains.
    pub async fn record_pair_with_detail(
        &self,
        a: &str,
        b: &str,
        score: f32,
        detail: &str,
    ) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query!(
            "INSERT OR IGNORE INTO artifact_pairs (a_id, b_id, score, state, detail, created_at)
             VALUES (?, ?, ?, 'pending', ?, ?)",
            a,
            b,
            score as f64,
            detail,
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// A real decision is left alone; `pending` is the absence of one and is answered here.
    pub async fn record_settled_pair(
        &self,
        a: &str,
        b: &str,
        score: f32,
        state: PairState,
    ) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query!(
            "INSERT INTO artifact_pairs (a_id, b_id, score, state, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(a_id, b_id) DO UPDATE SET state = excluded.state
               WHERE artifact_pairs.state = 'pending'",
            a,
            b,
            score as f64,
            state.as_str(),
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// For producers that file before acting. `from` narrows the write so a decision landing
    /// meanwhile is never reopened; `decided_by` clears with the state.
    pub async fn unsettle_pair(&self, a: &str, b: &str, from: PairState) -> Result<bool> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let res = sqlx::query!(
            "UPDATE artifact_pairs SET state = 'pending', decided_by = NULL
              WHERE a_id = ? AND b_id = ? AND state = ?",
            a,
            b,
            from.as_str()
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// A non-`pending` row carries a decision that a local rule must not overrule.
    pub async fn pair_state_between(&self, a: &str, b: &str) -> Result<Option<PairState>> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let state: Option<String> = sqlx::query_scalar!(
            "SELECT state FROM artifact_pairs WHERE a_id = ? AND b_id = ?",
            a,
            b
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(state.as_deref().map(PairState::parse))
    }

    pub async fn get_pair(&self, id: i64) -> Result<ArtifactPair> {
        let row = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT * FROM artifact_pairs WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(crate::error::Error::NotFound)?;
        Ok(row_to_pair(row))
    }

    pub async fn pairs_by_state(&self, state: PairState, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT * FROM artifact_pairs WHERE state = ?
              ORDER BY score DESC, created_at DESC LIMIT ?",
            state.as_str(),
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pair).collect())
    }

    /// All counts are pairs, not journal rows (a merge writes one per original), over one set:
    /// pairs a verdict settled, or whose verdict led to a journal row. Operator asks excluded.
    pub async fn band_record(&self, lo: f32, hi: f32) -> Result<BandRecord> {
        let r = sqlx::query!(
            "WITH acts AS (
               SELECT json_extract(evidence_json, '$.pair_id') AS pair_id,
                      MAX(undone_at IS NOT NULL) AS undone
                 FROM corpus_actions
                WHERE job = 'dedupe'
                  AND json_extract(evidence_json, '$.pair_id') IS NOT NULL
                  AND json_extract(evidence_json, '$.asked_by') IS NULL
                GROUP BY json_extract(evidence_json, '$.pair_id'))
             SELECT COUNT(*) AS judged,
                    COUNT(acts.pair_id) AS acted,
                    COALESCE(SUM(acts.undone), 0) AS undone
               FROM artifact_pairs p
               LEFT JOIN acts ON acts.pair_id = p.id
              WHERE p.score >= ? AND p.score < ? AND p.state <> 'pending'
                AND (p.decided_by IN ('model', 'evidence') OR acts.pair_id IS NOT NULL)",
            lo,
            hi
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(BandRecord {
            judged: r.judged as usize,
            acted: r.acted as usize,
            undone: r.undone as usize,
        })
    }

    /// Read-side only: a row naming a non-active artifact has no answer available.
    /// `follow_supersession` and `stale_unreachable_pairs` fix the rows themselves.
    pub async fn pairs_awaiting_review(
        &self,
        state: PairState,
        limit: i64,
    ) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT p.* FROM artifact_pairs p
               JOIN artifacts a ON a.id = p.a_id
               JOIN artifacts b ON b.id = p.b_id
              WHERE p.state = ?
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
              ORDER BY p.score DESC, p.created_at DESC LIMIT ?",
            state.as_str(),
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pair).collect())
    }

    /// Clears `obsolete_id` and `merged_into`, which belong to other states: a dismissed pair
    /// must not still offer a rejected supersede.
    pub async fn set_pair_state(
        &self,
        id: i64,
        state: PairState,
        detail: Option<&str>,
        by: DecidedBy,
    ) -> Result<()> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = ?, detail = ?, decided_by = ?, obsolete_id = NULL, merged_into = NULL
              WHERE id = ?",
            state.as_str(),
            detail,
            by.as_str(),
            id
        )
        .execute(&self.pool)
        .await?;
        // A stale Ops page may name a pair that is gone; reporting success would miscount.
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// The only path that writes `obsolete_id`. `by` is required: NULL means a legacy row.
    pub async fn set_pair_superseded(
        &self,
        id: i64,
        obsolete_id: &str,
        detail: Option<&str>,
        by: DecidedBy,
    ) -> Result<()> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'superseded', detail = ?, decided_by = ?,
                    obsolete_id = ?, merged_into = NULL
              WHERE id = ?",
            detail,
            by.as_str(),
            obsolete_id,
            id
        )
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// The press is the judgement; the dedupe unit then uses the write-only prompt, since the
    /// judge is unreliable on this class. Only while awaiting review, checked in the statement.
    pub async fn ask_pair_synthesis(&self, id: i64) -> Result<bool> {
        let holes = vec!["?"; AWAITING_REVIEW.len()].join(", ");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE artifact_pairs SET synthesis_asked = 1 WHERE id = ? AND state IN ({holes})"
        )))
        .bind(id);
        for state in AWAITING_REVIEW {
            q = q.bind(state.as_str());
        }
        Ok(q.execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Moves an older base's person-waiting rows into self-answered states; idempotent, run at
    /// the head of every reconcile sweep. `Oversized` is left to `reopen_oversized`.
    pub async fn drain_waiting_pairs(&self) -> Result<DrainCounts> {
        let mut tx = self.pool.begin().await?;
        let rejudged = sqlx::query!(
            "UPDATE artifact_pairs SET state = 'pending', decided_by = NULL
              WHERE (state = 'duplicate' AND merged_into IS NULL AND synthesis_asked = 0)
                 OR state = 'superseded'
                 OR state = 'vacuous'"
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let closed = sqlx::query!(
            "UPDATE artifact_pairs SET state = 'no_conflict', synthesis_asked = 0
              WHERE state = 'unmergeable'
                 OR (state = 'contradiction' AND detail LIKE '%esolve by hand%')
                 OR (state = 'contradiction'
                     AND detail LIKE '%merging them would have dropped one of them%')"
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(DrainCounts { closed, rejudged })
    }

    /// Called when the merge path refuses the draft.
    pub async fn clear_pair_synthesis(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifact_pairs SET synthesis_asked = 0 WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Not by state: `synthesis_asked` says the writing is owed, `merged_into` whether done.
    /// Held back past `MAX_UNREADABLE_JUDGEMENTS`, like `pairs_to_judge`.
    pub async fn pairs_awaiting_synthesis(&self, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT * FROM artifact_pairs
              WHERE synthesis_asked = 1 AND merged_into IS NULL
                AND judge_unreadable < ?
              ORDER BY judge_attempts ASC, score DESC, created_at DESC LIMIT ?",
            MAX_UNREADABLE_JUDGEMENTS,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pair).collect())
    }

    /// `by` is required: the unattended merge must not land as `decided_by = NULL`.
    pub async fn set_pair_merged(
        &self,
        id: i64,
        merged_into: &str,
        detail: Option<&str>,
        by: DecidedBy,
    ) -> Result<()> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'no_conflict', detail = ?, decided_by = ?,
                    merged_into = ?, obsolete_id = NULL
              WHERE id = ?",
            detail,
            by.as_str(),
            merged_into,
            id
        )
        .execute(&self.pool)
        .await?;
        if res.rows_affected() == 0 {
            return Err(crate::error::Error::NotFound);
        }
        Ok(())
    }

    /// `NoConflict`, not Pending: re-asking would regenerate the same unembeddable draft.
    /// The rule is the sweep's, so `decided_by` becomes the model.
    pub async fn reopen_pairs_merged_into(&self, merged_id: &str, detail: &str) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'no_conflict', detail = ?, decided_by = 'model',
                    merged_into = NULL
              WHERE merged_into = ?",
            detail,
            merged_id
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// By recorded lineage: before the embed lands `pairs_among` finds nothing. Dismissed, so
    /// the undo binds; `by` keeps both undo paths attributed alike.
    pub async fn dismiss_pairs_merged_into(
        &self,
        merged_id: &str,
        detail: &str,
        by: DecidedBy,
    ) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'dismissed', detail = ?, decided_by = ?, merged_into = NULL
              WHERE merged_into = ?",
            detail,
            by.as_str(),
            merged_id
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Self-pairs and collisions are dismissed rather than moved; only `Pending` remains by
    /// now. Counters reset; `score` goes stale but only orders the queue.
    pub async fn repoint_open_pairs(&self, old: &[String], new_id: &str) -> Result<u64> {
        let mut moved = 0u64;
        for o in old {
            if o == new_id {
                continue;
            }
            let rows = sqlx::query_as!(
                crate::store::pairs::PairRow,
                "SELECT * FROM artifact_pairs
                  WHERE state = 'pending' AND (a_id = ? OR b_id = ?)",
                o,
                o
            )
            .fetch_all(&self.pool)
            .await?;
            for p in rows.into_iter().map(row_to_pair) {
                let other = if p.a_id == *o {
                    p.b_id.clone()
                } else {
                    p.a_id.clone()
                };
                // Tested against `old` too: sides are visited one at a time.
                if other == new_id || old.contains(&other) {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("both of these went into the same merge"),
                        DecidedBy::Model,
                    )
                    .await?;
                    continue;
                }
                if self.pair_state_between(&other, new_id).await?.is_some() {
                    self.set_pair_state(
                        p.id,
                        PairState::Dismissed,
                        Some("a pair between these two already exists"),
                        DecidedBy::Model,
                    )
                    .await?;
                    continue;
                }
                let (a, b) = if other.as_str() <= new_id {
                    (other.as_str(), new_id)
                } else {
                    (new_id, other.as_str())
                };
                sqlx::query!(
                    "UPDATE artifact_pairs
                        SET a_id = ?, b_id = ?, judge_attempts = 0, judge_unreadable = 0,
                            detail = NULL
                      WHERE id = ?",
                    a,
                    b,
                    p.id
                )
                .execute(&self.pool)
                .await?;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// For the one caller that acts on the row itself.
    pub async fn pair_between(&self, a: &str, b: &str) -> Result<Option<ArtifactPair>> {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let row = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT * FROM artifact_pairs WHERE a_id = ? AND b_id = ?",
            a,
            b
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_pair))
    }

    /// Only while both sides are in results.
    pub async fn open_contradictions(&self, ids: &[String]) -> Result<Vec<Disagreement>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let holes = vec!["?"; ids.len()].join(", ");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT p.a_id, p.b_id, p.detail,
                    a.title AS a_title, a.created_at AS a_created,
                    b.title AS b_title, b.created_at AS b_created
               FROM artifact_pairs p
               JOIN artifacts a ON a.id = p.a_id
               JOIN artifacts b ON b.id = p.b_id
              WHERE p.state = 'contradiction'
                AND a.status = 'active' AND a.superseded_by IS NULL
                AND b.status = 'active' AND b.superseded_by IS NULL
                AND (p.a_id IN ({holes}) OR p.b_id IN ({holes}))
              ORDER BY p.id"
        )));
        for _ in 0..2 {
            for id in ids {
                q = q.bind(id);
            }
        }
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        for r in q.fetch_all(&self.pool).await? {
            let a_id: String = r.get("a_id");
            let b_id: String = r.get("b_id");
            let detail: Option<String> = r.get("detail");
            if wanted.contains(a_id.as_str()) {
                out.push(Disagreement {
                    artifact_id: a_id.clone(),
                    created_at: r.get("a_created"),
                    other_id: b_id.clone(),
                    other_title: r.get("b_title"),
                    other_created_at: r.get("b_created"),
                    detail: detail.clone(),
                });
            }
            if wanted.contains(b_id.as_str()) {
                out.push(Disagreement {
                    artifact_id: b_id,
                    created_at: r.get("b_created"),
                    other_id: a_id,
                    other_title: r.get("a_title"),
                    other_created_at: r.get("a_created"),
                    detail,
                });
            }
        }
        Ok(out)
    }

    /// Counters, detail and `decided_by` reset: the question is asked again from nothing.
    async fn reopen_pair(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL, obsolete_id = NULL, merged_into = NULL,
                    decided_by = NULL, judge_attempts = 0, judge_unreadable = 0
              WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The still-open states, written once for the three queries that carry, list and repair
    /// pairs. A state missing here is a question that quietly stops being asked.
    const OPEN_STATES: &str = "('pending', 'contradiction', 'superseded', 'vacuous', 'duplicate')";

    /// Moves `loser`'s open pairs onto `winner`; self-pairs, spent proposals, collisions and
    /// `Vacuous` verdicts settle `Stale` instead, which `reopen_stale_pairs` undoes.
    pub async fn follow_supersession(&self, loser: &str, winner: &str) -> Result<Followed> {
        if loser == winner {
            return Ok(Followed::default());
        }
        // runtime: shares OPEN_STATES with the other open-pair statements.
        let rows = sqlx::query_as::<_, PairRow>(sqlx::AssertSqlSafe(format!(
            "SELECT * FROM artifact_pairs
              WHERE state IN {}
                AND (a_id = ? OR b_id = ?)",
            Self::OPEN_STATES
        )))
        .bind(loser)
        .bind(loser)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Followed::default();
        for p in rows.into_iter().map(row_to_pair) {
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
                    DecidedBy::Model,
                )
                .await?;
                out.staled += 1;
                continue;
            }
            if p.obsolete_id.as_deref() == Some(loser) {
                self.set_pair_state(
                    p.id,
                    PairState::Stale,
                    Some("the artifact this proposed hiding is already hidden"),
                    DecidedBy::Model,
                )
                .await?;
                out.staled += 1;
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
                    DecidedBy::Model,
                )
                .await?;
                out.staled += 1;
                continue;
            }
            if let Some(existing) = self.pair_between(&other, winner).await? {
                // A contradicted machine verdict is reopened so the disagreement is not lost.
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
                    DecidedBy::Model,
                )
                .await?;
                out.staled += 1;
                continue;
            }
            let (a, b) = if other.as_str() <= winner {
                (other.as_str(), winner)
            } else {
                (winner, other.as_str())
            };
            if p.state == PairState::Pending {
                sqlx::query!(
                    "UPDATE artifact_pairs
                        SET a_id = ?, b_id = ?, judge_attempts = 0, judge_unreadable = 0,
                            detail = NULL
                      WHERE id = ?",
                    a,
                    b,
                    p.id
                )
                .execute(&self.pool)
                .await?;
            } else {
                // Prose, no ids: Capture renders `detail` verbatim beside titled artifacts.
                let note = "carried over from the artifact this one superseded";
                let detail = match p.detail.as_deref() {
                    Some(d) => format!("{d} ({note})"),
                    None => note.to_string(),
                };
                sqlx::query!(
                    "UPDATE artifact_pairs SET a_id = ?, b_id = ?, detail = ? WHERE id = ?",
                    a,
                    b,
                    detail,
                    p.id
                )
                .execute(&self.pool)
                .await?;
            }
            out.moved += 1;
        }
        Ok(out)
    }

    /// After the repair pass, and only for pairs whose chain does not end in results: those
    /// the repair still owes a move. `decided_by` becomes the model.
    pub async fn stale_unreachable_pairs(&self) -> Result<u64> {
        // runtime: shares OPEN_STATES with the other open-pair statements.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT p.id AS pair_id, x.id AS side, x.superseded_by AS winner
               FROM artifact_pairs p
               JOIN artifacts x ON x.id IN (p.a_id, p.b_id)
              WHERE p.state IN {}
                AND (x.status <> 'active' OR x.superseded_by IS NOT NULL) AND x.status <> 'trashed'",
            Self::OPEN_STATES
        )))
        .fetch_all(&self.pool)
        .await?;

        // One walk per superseded artifact, not per pair naming it.
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
        // Chunked under SQLITE_MAX_VARIABLE_NUMBER (999 before 3.32).
        for chunk in to_settle.chunks(500) {
            let holes = vec!["?"; chunk.len()].join(", ");
            // runtime: one placeholder per item in the IN list.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE artifact_pairs
                    SET state = 'stale', obsolete_id = NULL, merged_into = NULL,
                        decided_by = 'model',
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

    /// `Pending`, not the old state: the settlement reasons are void. Only where the other side
    /// is in results too; `decided_by` clears.
    pub async fn reopen_stale_pairs(&self, artifact_id: &str) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL, obsolete_id = NULL, merged_into = NULL,
                    decided_by = NULL, judge_attempts = 0, judge_unreadable = 0
              WHERE state = 'stale'
                AND (a_id = ?1 OR b_id = ?1)
                AND NOT EXISTS (
                      SELECT 1 FROM artifacts x
                       WHERE x.id IN (artifact_pairs.a_id, artifact_pairs.b_id)
                         AND (x.status <> 'active' OR x.superseded_by IS NOT NULL) AND x.status <> 'trashed')",
            artifact_id
        )
            .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Reports the chain's end, not the first hop. Chains ending outside results yield nothing;
    /// `stale_unreachable_pairs` settles those.
    pub async fn supersessions_with_open_pairs(&self, limit: i64) -> Result<Vec<(String, String)>> {
        // runtime: shares OPEN_STATES with the other open-pair statements.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT loser.id AS loser, loser.superseded_by AS winner
               FROM artifacts loser
               JOIN artifact_pairs p ON p.a_id = loser.id OR p.b_id = loser.id
              WHERE loser.superseded_by IS NOT NULL
                AND p.state IN {}
              LIMIT ?",
            Self::OPEN_STATES
        )))
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

    /// Bounded and cycle-guarded: a hung sweep would be worse than the drift.
    async fn end_of_supersession_chain(&self, from: &str) -> Result<Option<String>> {
        const MAX_HOPS: usize = 32;
        let mut at = from.to_string();
        let mut seen = vec![at.clone()];
        for _ in 0..MAX_HOPS {
            let row = sqlx::query!(
                "SELECT status, superseded_by FROM artifacts WHERE id = ?",
                &at
            )
            .fetch_optional(&self.pool)
            .await?;
            let Some(row) = row else { return Ok(None) };
            match row.superseded_by {
                Some(next) => {
                    if seen.contains(&next) {
                        tracing::warn!(artifact = %at, "a supersession chain loops");
                        return Ok(None);
                    }
                    seen.push(next.clone());
                    at = next;
                }
                None => {
                    return Ok((row.status == "active").then_some(at));
                }
            }
        }
        tracing::warn!(artifact = %from, "a supersession chain is longer than the repair follows");
        Ok(None)
    }

    /// Idempotent, so it runs every sweep. Counters and `decided_by` reset, or a pair reopened
    /// past `MAX_UNREADABLE_JUDGEMENTS` would sit pending for good.
    pub async fn reopen_oversized(&self) -> Result<u64> {
        let res = sqlx::query!(
            "UPDATE artifact_pairs
                SET state = 'pending', detail = NULL, decided_by = NULL,
                    judge_attempts = 0, judge_unreadable = 0
              WHERE state = 'oversized'"
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Least-attempted first, then score, so unreadable pairs do not absorb every budget.
    /// Past `MAX_UNREADABLE_JUDGEMENTS` held back: they stay on the operator's queue.
    pub async fn pairs_to_judge(&self, limit: i64) -> Result<Vec<ArtifactPair>> {
        let rows = sqlx::query_as!(
            crate::store::pairs::PairRow,
            "SELECT * FROM artifact_pairs
              WHERE state = 'pending' AND judge_unreadable < ?
              ORDER BY judge_attempts ASC, score DESC, created_at DESC LIMIT ?",
            MAX_UNREADABLE_JUDGEMENTS,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_pair).collect())
    }

    /// Computed at use, not when armed. `Pending` only: settled rows carry decisions. The seed is
    /// read directly and joins the window if missing, or it loops with an empty component.
    pub async fn open_component(&self, pair_id: i64) -> Result<Vec<ArtifactPair>> {
        let mut open = self
            .pairs_by_state(PairState::Pending, COMPONENT_WINDOW)
            .await?;
        if !open.iter().any(|p| p.id == pair_id) {
            let seed = self.get_pair(pair_id).await?;
            // Settled while the unit waited: the one case where empty is right.
            if seed.state != PairState::Pending {
                return Ok(vec![]);
            }
            open.push(seed);
        }
        let seed = open
            .iter()
            .find(|p| p.id == pair_id)
            .ok_or(crate::error::Error::NotFound)?;

        // Adjacency once, then a flood fill from the seed's two artifacts.
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

    /// Written before the call, so a run dying mid-judgement still counts.
    pub async fn record_judge_attempt(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifact_pairs SET judge_attempts = judge_attempts + 1 WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stepped only on a parse failure: an outage must not shelve the queue.
    pub async fn record_unreadable_judgement(&self, id: i64) -> Result<()> {
        sqlx::query!(
            "UPDATE artifact_pairs SET judge_unreadable = judge_unreadable + 1 WHERE id = ?",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pair_remembers_that_a_person_asked_for_a_synthesis() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].clone();
        assert!(!p.synthesis_asked, "nobody has asked yet");

        s.ask_pair_synthesis(p.id).await.unwrap();
        let after = s.get_pair(p.id).await.unwrap();
        assert!(after.synthesis_asked);
        assert_eq!(
            after.state,
            PairState::Pending,
            "the judge's finding is not overwritten by the operator's ask"
        );

        s.clear_pair_synthesis(p.id).await.unwrap();
        assert!(!s.get_pair(p.id).await.unwrap().synthesis_asked);
    }

    #[tokio::test]
    async fn a_band_record_counts_judged_pairs_and_the_journal_s_actions_in_its_score_range() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let s = Store::memory().await.unwrap();
        let mut ids = Vec::new();
        for (i, score) in [0.81f32, 0.86, 0.90, 0.82].into_iter().enumerate() {
            let (a, b) = two_artifacts(&s).await;
            s.record_pair(&a, &b, score).await.unwrap();
            let id = s
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|p| (p.score - score).abs() < 1e-6)
                .unwrap()
                .id;
            if i < 3 {
                s.set_pair_state(id, PairState::Dismissed, None, DecidedBy::Model)
                    .await
                    .unwrap();
            }
            ids.push(id);
        }
        let row = |score: f32, pair: i64, subject: &str| NewAction {
            job: Job::Dedupe,
            kind: Kind::Supersede,
            subject_id: subject.to_string(),
            survivor_id: None,
            detail: None,
            evidence: serde_json::json!({ "pair_id": pair }),
            pair_score: Some(score),
        };
        s.record_action(&row(0.81, ids[0], "s0.81")).await.unwrap();
        s.record_action(&row(0.90, ids[2], "s0.90")).await.unwrap();
        s.undo_action_on("s0.81", Kind::Supersede, UndoneBy::Operator, "back")
            .await
            .unwrap();
        // Both rows name the same pair, so the band counts one action.
        s.record_action(&row(0.86, ids[1], "s0.86-a"))
            .await
            .unwrap();
        s.record_action(&row(0.86, ids[1], "s0.86-b"))
            .await
            .unwrap();

        assert_eq!(
            s.band_record(0.80, 0.88).await.unwrap(),
            BandRecord {
                judged: 2,
                acted: 2,
                undone: 1
            },
            "the pending pair is not judged, and one pair is one action"
        );
        assert_eq!(
            s.band_record(0.88, 1.0).await.unwrap(),
            BandRecord {
                judged: 1,
                acted: 1,
                undone: 0
            }
        );
    }

    #[tokio::test]
    async fn a_band_record_counts_an_operators_undo_and_not_an_operators_press() {
        use crate::store::actions::{Job, Kind, NewAction, UndoneBy};
        let s = Store::memory().await.unwrap();
        let mut ids = Vec::new();
        for score in [0.84f32, 0.85] {
            let (a, b) = two_artifacts(&s).await;
            s.record_pair(&a, &b, score).await.unwrap();
            let id = s
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|p| (p.score - score).abs() < 1e-6)
                .unwrap()
                .id;
            ids.push(id);
        }
        let row = |subject: &str, evidence: serde_json::Value| NewAction {
            job: Job::Dedupe,
            kind: Kind::Supersede,
            subject_id: subject.to_string(),
            survivor_id: None,
            detail: None,
            evidence,
            pair_score: Some(0.84),
        };
        s.set_pair_state(ids[0], PairState::Superseded, None, DecidedBy::Model)
            .await
            .unwrap();
        s.record_action(&row("judged", serde_json::json!({ "pair_id": ids[0] })))
            .await
            .unwrap();
        s.undo_action_on("judged", Kind::Supersede, UndoneBy::Operator, "back")
            .await
            .unwrap();
        s.set_pair_state(ids[0], PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();
        s.set_pair_state(ids[1], PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();
        s.record_action(&row(
            "asked",
            serde_json::json!({ "pair_id": ids[1], "asked_by": "operator" }),
        ))
        .await
        .unwrap();

        assert_eq!(
            s.band_record(0.80, 0.88).await.unwrap(),
            BandRecord {
                judged: 1,
                acted: 1,
                undone: 1
            },
            "the undone pair left `judged` and the press counted as an action"
        );
    }
    use crate::store::Store;
    use crate::store::artifacts::NewArtifact;

    async fn n_artifacts(s: &Store, n: usize) -> Vec<String> {
        let src = s.insert_corpus("x", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..n)
            .map(|i| NewArtifact {
                ordinal: i as i64,
                text: format!("artifact {i}"),
                ..Default::default()
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
                        text: "one".into(),
                        ..Default::default()
                    },
                    NewArtifact {
                        ordinal: 1,
                        text: "two".into(),
                        ..Default::default()
                    },
                ],
            )
            .await
            .unwrap();
        (made[0].id.clone(), made[1].id.clone())
    }

    #[tokio::test]
    async fn a_contradiction_is_read_from_both_sides_while_both_are_live() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.9).await.unwrap();
        let id = s.pair_between(&a, &b).await.unwrap().unwrap().id;
        s.set_pair_state(
            id,
            PairState::Contradiction,
            Some("30 days there, 14 here"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        let got = s
            .open_contradictions(&[a.clone(), b.clone()])
            .await
            .unwrap();
        assert_eq!(got.len(), 2, "one row per side asked about");
        let from_a = got.iter().find(|d| d.artifact_id == a).unwrap();
        assert_eq!(from_a.other_id, b);
        assert_eq!(from_a.detail.as_deref(), Some("30 days there, 14 here"));
        let from_b = got.iter().find(|d| d.artifact_id == b).unwrap();
        assert_eq!(from_b.other_id, a);

        assert_eq!(
            s.open_contradictions(std::slice::from_ref(&a))
                .await
                .unwrap()
                .len(),
            1,
            "only the side asked about gets a row"
        );

        s.set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert!(
            s.open_contradictions(&[a]).await.unwrap().is_empty(),
            "a side out of results is not something to disagree with"
        );
    }

    #[tokio::test]
    async fn an_artifact_disagreeing_with_two_lists_both() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        for other in &ids[1..] {
            s.record_pair(&ids[0], other, 0.9).await.unwrap();
            let id = s.pair_between(&ids[0], other).await.unwrap().unwrap().id;
            s.set_pair_state(
                id,
                PairState::Contradiction,
                Some("differs"),
                DecidedBy::Model,
            )
            .await
            .unwrap();
        }
        let got = s.open_contradictions(&[ids[0].clone()]).await.unwrap();
        let mut others: Vec<&str> = got.iter().map(|d| d.other_id.as_str()).collect();
        others.sort();
        let mut want: Vec<&str> = ids[1..].iter().map(String::as_str).collect();
        want.sort();
        assert_eq!(others, want);
    }

    #[tokio::test]
    async fn a_pair_the_judge_can_never_read_stops_costing_calls() {
        // The unit closes at `MAX_ATTEMPTS`; without a ceiling each sweep re-arms it for five more.
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
        // Attempts count before the call, so gating on them would let an outage empty the queue.
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
        // The >= auto_supersede band is settled free by clustering; it must not reach the judge.
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
    async fn who_settled_a_pair_is_recorded_rather_than_reconstructed() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        assert_eq!(
            s.get_pair(p).await.unwrap().decided_by,
            None,
            "an open pair has been decided by nobody"
        );

        s.set_pair_state(p, PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();

        assert_eq!(
            s.get_pair(p).await.unwrap().decided_by,
            Some(DecidedBy::Operator)
        );
    }

    #[tokio::test]
    async fn reopening_a_pair_takes_the_old_decider_off_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            p,
            PairState::Stale,
            Some("its side was hidden"),
            DecidedBy::Operator,
        )
        .await
        .unwrap();

        assert_eq!(s.reopen_stale_pairs(&a).await.unwrap(), 1);

        let read = s.get_pair(p).await.unwrap();
        assert_eq!(read.state, PairState::Pending);
        assert_eq!(
            read.decided_by, None,
            "an open pair still named the person who answered a different question"
        );
    }

    #[tokio::test]
    async fn reopening_an_oversized_pair_takes_the_old_decider_off_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(p, PairState::Oversized, Some("9 roots"), DecidedBy::Model)
            .await
            .unwrap();

        assert_eq!(s.reopen_oversized().await.unwrap(), 1);

        assert_eq!(s.get_pair(p).await.unwrap().decided_by, None);
    }

    #[tokio::test]
    async fn unsettling_a_pair_takes_the_old_decider_off_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(p, PairState::Dismissed, None, DecidedBy::Operator)
            .await
            .unwrap();

        assert!(s.unsettle_pair(&a, &b, PairState::Dismissed).await.unwrap());

        assert_eq!(s.get_pair(p).await.unwrap().decided_by, None);
    }

    #[tokio::test]
    async fn a_row_written_before_the_column_existed_claims_no_author() {
        // Nullable, no default: older rows must not assert an author.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        sqlx::query!(
            "UPDATE artifact_pairs SET state = 'dismissed', decided_by = NULL WHERE id = ?",
            p
        )
        .execute(&s.pool)
        .await
        .unwrap();

        let read = s.get_pair(p).await.unwrap();
        assert_eq!(read.state, PairState::Dismissed);
        assert_eq!(read.decided_by, None, "silence is not a claim about anyone");
    }

    #[tokio::test]
    async fn an_oversized_pair_leaves_the_pending_queue_but_stays_visible() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            p,
            PairState::Oversized,
            Some("9 roots, cap is 8"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        assert!(s.pairs_to_judge(10).await.unwrap().is_empty());
        let found = s.pairs_by_state(PairState::Oversized, 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("9 roots, cap is 8"));
    }

    #[tokio::test]
    async fn every_state_survives_a_round_trip_through_the_database() {
        // `parse` falls back to Pending, so a missing string would be asked about forever.
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
                s.set_pair_state(p, state, None, DecidedBy::Model)
                    .await
                    .unwrap();
            }
            assert_eq!(
                s.get_pair(p).await.unwrap().state,
                state,
                "{} did not survive the round trip",
                state.as_str()
            );
        }
    }

    async fn four_artifacts(s: &Store) -> Vec<String> {
        let src = s.insert_corpus("four", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = (0..4)
            .map(|i| NewArtifact {
                ordinal: i,
                text: format!("artifact {i}"),
                ..Default::default()
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
        // Must not depend on row order, or two units could act on different sets.
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
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        s.record_pair(&m[1], &m[2], 0.90).await.unwrap();
        let all = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        let (seed, other) = (all[0].id, all[1].id);
        s.set_pair_state(other, PairState::Dismissed, None, DecidedBy::Model)
            .await
            .unwrap();

        assert_eq!(s.open_component(seed).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pair_below_the_window_is_still_the_seed_of_its_own_component() {
        // Lowest-scoring seeds fall outside the window and must still get their component.
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
        let s = Store::memory().await.unwrap();
        let m = four_artifacts(&s).await;
        s.record_pair(&m[0], &m[1], 0.91).await.unwrap();
        let p = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(p, PairState::NoConflict, None, DecidedBy::Model)
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
            DecidedBy::Model,
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
        // The sweep re-finds the pair every run; a dismissal must survive.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_state(p.id, PairState::Dismissed, None, DecidedBy::Model)
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
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let p = s
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_superseded(p.id, &a, Some("a is stale"), DecidedBy::Model)
            .await
            .unwrap();
        assert_eq!(
            s.get_pair(p.id).await.unwrap().obsolete_id.as_deref(),
            Some(a.as_str())
        );

        s.set_pair_state(p.id, PairState::Dismissed, None, DecidedBy::Model)
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

        let p = s
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap()
            .remove(0);
        s.set_pair_state(p.id, PairState::Dismissed, None, DecidedBy::Model)
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
        // Reporting success would miscount the queue.
        let s = Store::memory().await.unwrap();
        assert!(matches!(
            s.set_pair_state(9999, PairState::Dismissed, None, DecidedBy::Model)
                .await,
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
                ..Default::default()
            })
            .collect();
        let made = s.insert_artifacts(&src.id, &new).await.unwrap();
        let (a, b, c, d) = (
            made[0].id.clone(),
            made[1].id.clone(),
            made[2].id.clone(),
            made[3].id.clone(),
        );
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
        // The producer files before acting; a failed action must reopen the pair.
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
        // A decision landing between settlement and failure must not be undone.
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_settled_pair(&a, &b, 0.99, PairState::NoConflict)
            .await
            .unwrap();
        let id = s.pairs_by_state(PairState::NoConflict, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Dismissed, None, DecidedBy::Model)
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

        s.set_pair_merged(id, "merge-1", Some("same claim"), DecidedBy::Model)
            .await
            .unwrap();

        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.merged_into.as_deref(), Some("merge-1"));

        s.set_pair_state(id, PairState::Dismissed, None, DecidedBy::Model)
            .await
            .unwrap();
        assert_eq!(s.get_pair(id).await.unwrap().merged_into, None);
    }

    #[tokio::test]
    async fn an_applied_merge_says_who_applied_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;

        s.set_pair_merged(id, "merge-1", None, DecidedBy::Model)
            .await
            .unwrap();
        assert_eq!(
            s.get_pair(id).await.unwrap().decided_by,
            Some(DecidedBy::Model)
        );

        s.set_pair_superseded(id, &a, None, DecidedBy::Operator)
            .await
            .unwrap();
        assert_eq!(
            s.get_pair(id).await.unwrap().decided_by,
            Some(DecidedBy::Operator),
            "the proposed direction was recorded as nobody's"
        );
    }

    #[tokio::test]
    async fn dismissing_a_merge_s_pairs_carries_the_name_the_undo_gives_it() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_merged(id, "merge-1", None, DecidedBy::Model)
            .await
            .unwrap();

        assert_eq!(
            s.dismiss_pairs_merged_into("merge-1", "merge undone", DecidedBy::Operator)
                .await
                .unwrap(),
            1
        );
        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::Dismissed);
        assert_eq!(p.decided_by, Some(DecidedBy::Operator));
    }

    #[tokio::test]
    async fn reopening_a_stranded_merge_s_pairs_touches_only_its_own() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_merged(id, "merge-1", None, DecidedBy::Model)
            .await
            .unwrap();

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
        assert_eq!(p.state, PairState::NoConflict);
        assert_eq!(p.merged_into, None);
        assert_eq!(
            p.decided_by,
            Some(DecidedBy::Model),
            "the reap is a rule the sweep applied on its own; the row has to say so"
        );
    }

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

    #[tokio::test]
    async fn re_pointing_onto_an_existing_pair_leaves_the_existing_row_alone() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (b, c, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(c, m, 0.80).await.unwrap();
        let existing = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            existing,
            PairState::Dismissed,
            Some("operator"),
            DecidedBy::Model,
        )
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

    #[tokio::test]
    async fn a_settled_pair_is_not_re_pointed() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (b, c, m) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(b, c, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::NoConflict, None, DecidedBy::Model)
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

    #[tokio::test]
    async fn an_oversized_pair_goes_back_into_the_queue() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            id,
            PairState::Oversized,
            Some("12 sources, cap is 8"),
            DecidedBy::Model,
        )
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
        assert_eq!(s.reopen_oversized().await.unwrap(), 0);
    }

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
        s.set_pair_state(
            id,
            PairState::Oversized,
            Some("12 sources, cap is 8"),
            DecidedBy::Model,
        )
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

    #[tokio::test]
    async fn an_open_pair_follows_its_member_into_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();

        assert_eq!(s.follow_supersession(loser, winner).await.unwrap().moved, 1);

        let open = s.pairs_by_state(PairState::Pending, 10).await.unwrap();
        assert_eq!(open.len(), 1);
        let mut sides = [open[0].a_id.clone(), open[0].b_id.clone()];
        sides.sort();
        let mut want = [other.clone(), winner.clone()];
        want.sort();
        assert_eq!(sides, want, "the pair still names the artifact that lost");
    }

    #[tokio::test]
    async fn a_pair_between_the_two_sides_of_a_supersession_is_settled() {
        let s = Store::memory().await.unwrap();
        let (loser, winner) = two_artifacts(&s).await;
        s.record_pair(&loser, &winner, 0.91).await.unwrap();

        let f = s.follow_supersession(&loser, &winner).await.unwrap();
        // `jobs::did_work` reads this; `moved` alone made the sweep back off with work left.
        assert_eq!((f.moved, f.staled), (0, 1));
        assert_eq!(f.settled(), 1);

        assert!(
            s.pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
        // `Stale`, not `Dismissed`: undoing the supersession has to bring it back.
        assert_eq!(
            s.pairs_by_state(PairState::Stale, 10).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn a_verdict_keeps_its_state_when_it_follows_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            id,
            PairState::Contradiction,
            Some("30 seconds vs 90"),
            DecidedBy::Model,
        )
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
        assert!(
            detail.contains("carried over"),
            "nothing says the verdict was carried over: {detail}"
        );
        assert!(
            !detail.contains(winner.as_str()) && !detail.contains(loser.as_str()),
            "the note put a raw id in front of a reader: {detail}"
        );
    }

    #[tokio::test]
    async fn a_verdict_with_nowhere_to_land_reopens_the_row_standing_in_its_place() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let verdict = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(
            verdict,
            PairState::Contradiction,
            Some("30 seconds vs 90"),
            DecidedBy::Model,
        )
        .await
        .unwrap();
        s.record_pair(other, winner, 0.72).await.unwrap();
        let standing = s.pair_between(other, winner).await.unwrap().unwrap().id;
        s.set_pair_state(
            standing,
            PairState::NoConflict,
            Some("nothing in common"),
            DecidedBy::Model,
        )
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

    #[tokio::test]
    async fn a_collision_leaves_a_dismissal_alone() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let verdict = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(
            verdict,
            PairState::Contradiction,
            Some("30 seconds vs 90"),
            DecidedBy::Model,
        )
        .await
        .unwrap();
        s.record_pair(other, winner, 0.72).await.unwrap();
        let standing = s.pair_between(other, winner).await.unwrap().unwrap().id;
        s.set_pair_state(
            standing,
            PairState::Dismissed,
            Some("not worth looking at"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        s.follow_supersession(loser, winner).await.unwrap();

        assert_eq!(
            s.get_pair(standing).await.unwrap().state,
            PairState::Dismissed,
            "a supersession overruled an operator's dismissal"
        );
    }

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
            DecidedBy::Model,
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

    #[tokio::test]
    async fn a_duplicate_pair_is_carried_onto_the_winner_like_any_other_open_one() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (a, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(a, other, 0.91).await.unwrap();
        let id = s.pair_between(a, other).await.unwrap().unwrap().id;
        s.set_pair_state(
            id,
            PairState::Duplicate,
            Some("covers the same ground"),
            DecidedBy::Model,
        )
        .await
        .unwrap();
        s.set_superseded_by(a, Some(winner)).await.unwrap();

        assert_eq!(
            s.supersessions_with_open_pairs(10).await.unwrap(),
            vec![(a.clone(), winner.clone())],
            "a duplicate awaiting a press was invisible to the repair"
        );
        s.follow_supersession(a, winner).await.unwrap();
        let moved = s.pair_between(winner, other).await.unwrap();
        assert!(
            moved.is_some(),
            "the question was not carried onto the artifact that now answers for it"
        );
    }

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

    #[tokio::test]
    async fn going_stale_does_not_leave_an_operator_s_name_on_the_row() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(id, PairState::Contradiction, None, DecidedBy::Operator)
            .await
            .unwrap();
        s.set_artifact_status(&b, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        assert_eq!(s.stale_unreachable_pairs().await.unwrap(), 1);
        let p = s.get_pair(id).await.unwrap();
        assert_eq!(p.state, PairState::Stale);
        assert_eq!(p.decided_by, Some(DecidedBy::Model));
    }

    #[tokio::test]
    async fn a_question_the_repair_still_owes_a_move_is_not_settled_first() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pair_between(loser, other).await.unwrap().unwrap().id;
        s.set_pair_state(
            id,
            PairState::Contradiction,
            Some("30 seconds vs 90"),
            DecidedBy::Model,
        )
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

        assert_eq!(s.follow_supersession(loser, winner).await.unwrap().moved, 1);
        let moved = s.get_pair(id).await.unwrap();
        assert_eq!(moved.state, PairState::Contradiction);
        assert!(
            [&moved.a_id, &moved.b_id].contains(&winner),
            "the verdict did not land on the winner"
        );
    }

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

    #[tokio::test]
    async fn a_verdict_nobody_can_act_on_comes_off_the_queue_and_back_again() {
        let s = Store::memory().await.unwrap();
        let (a, b) = two_artifacts(&s).await;
        s.record_pair(&a, &b, 0.91).await.unwrap();
        let id = s.pair_between(&a, &b).await.unwrap().unwrap().id;
        s.set_pair_state(
            id,
            PairState::Contradiction,
            Some("30 seconds vs 90"),
            DecidedBy::Model,
        )
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

    #[tokio::test]
    async fn a_proposal_to_hide_the_side_that_lost_is_settled() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_superseded(id, loser, Some("the older reading"), DecidedBy::Model)
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
            DecidedBy::Model,
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

    #[tokio::test]
    async fn a_settled_pair_is_not_dragged_into_a_supersession() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        let (loser, other, winner) = (&ids[0], &ids[1], &ids[2]);
        s.record_pair(loser, other, 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            id,
            PairState::NoConflict,
            Some("nothing in common"),
            DecidedBy::Model,
        )
        .await
        .unwrap();

        assert_eq!(
            s.follow_supersession(loser, winner)
                .await
                .unwrap()
                .settled(),
            0
        );

        let kept = s.get_pair(id).await.unwrap();
        assert_eq!(kept.state, PairState::NoConflict);
        assert!(kept.a_id == *loser || kept.b_id == *loser);
    }

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
    }

    #[tokio::test]
    async fn a_pair_naming_a_superseded_artifact_is_not_awaiting_review() {
        let s = Store::memory().await.unwrap();
        let ids = n_artifacts(&s, 3).await;
        s.record_pair(&ids[0], &ids[1], 0.91).await.unwrap();
        let id = s.pairs_by_state(PairState::Pending, 10).await.unwrap()[0].id;
        s.set_pair_state(
            id,
            PairState::Contradiction,
            Some("30 vs 90"),
            DecidedBy::Model,
        )
        .await
        .unwrap();
        s.set_superseded_by(&ids[0], Some(&ids[2])).await.unwrap();

        assert!(
            s.pairs_awaiting_review(PairState::Contradiction, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
