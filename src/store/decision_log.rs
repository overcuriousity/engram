//! The record of every background decision: what was asked, what came back, the band, what became of it.

use super::{Store, new_id, now};
use crate::core::judging::band::{Band, Thresholds, era};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Waiting,
    Open,
    Done,
    Lapsed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Open => "open",
            State::Done => "done",
            State::Lapsed => "lapsed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "waiting" => State::Waiting,
            "open" => State::Open,
            "done" => State::Done,
            "lapsed" => State::Lapsed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Kept,
    Undone,
    Confirmed,
    Corrected,
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Kept => "kept",
            Outcome::Undone => "undone",
            Outcome::Confirmed => "confirmed",
            Outcome::Corrected => "corrected",
            Outcome::Refused => "refused",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "kept" => Outcome::Kept,
            "undone" => Outcome::Undone,
            "confirmed" => Outcome::Confirmed,
            "corrected" => Outcome::Corrected,
            "refused" => Outcome::Refused,
            _ => return None,
        })
    }
}

fn parse_band(s: &str) -> Option<Band> {
    Some(match s {
        "act" => Band::Act,
        "ask" => Band::Ask,
        "leave" => Band::Leave,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct NewLogged {
    pub question_id: String,
    pub question_version: u32,
    pub model: String,
    pub subject_kind: &'static str,
    pub subject_a: String,
    pub subject_b: Option<String>,
    pub rev_a: Option<i64>,
    pub rev_b: Option<i64>,
    pub answer: String,
    pub top: f64,
    pub pick: String,
    pub swapped_agree: Option<bool>,
    pub band: Band,
    pub reason: Option<String>,
    pub state: State,
}

#[derive(Debug, Clone)]
pub struct Logged {
    pub id: String,
    pub question_id: String,
    pub question_version: i64,
    pub model: String,
    pub era: String,
    pub subject_kind: String,
    pub subject_a: String,
    pub subject_b: Option<String>,
    pub rev_a: Option<i64>,
    pub rev_b: Option<i64>,
    pub answer: String,
    pub top: f64,
    pub pick: String,
    pub swapped_agree: Option<bool>,
    pub band: Band,
    pub reason: Option<String>,
    pub state: State,
    pub outcome: Option<Outcome>,
    pub outcome_at: Option<i64>,
    pub answered_pick: Option<String>,
    pub decided_at: i64,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    question_id: String,
    question_version: i64,
    model: String,
    era: String,
    subject_kind: String,
    subject_a: String,
    subject_b: Option<String>,
    rev_a: Option<i64>,
    rev_b: Option<i64>,
    answer: String,
    top: f64,
    pick: String,
    swapped_agree: Option<i64>,
    band: String,
    reason: Option<String>,
    state: String,
    outcome: Option<String>,
    outcome_at: Option<i64>,
    answered_pick: Option<String>,
    decided_at: i64,
}

impl Row {
    fn read(self) -> Result<Logged> {
        let bad = |what: &str, v: &str| {
            crate::error::Error::Store(format!("decision {}: bad {what} {v:?}", self.id))
        };
        Ok(Logged {
            band: parse_band(&self.band).ok_or_else(|| bad("band", &self.band))?,
            state: State::parse(&self.state).ok_or_else(|| bad("state", &self.state))?,
            outcome: match &self.outcome {
                None => None,
                Some(o) => Some(Outcome::parse(o).ok_or_else(|| bad("outcome", o))?),
            },
            swapped_agree: self.swapped_agree.map(|v| v != 0),
            id: self.id,
            question_id: self.question_id,
            question_version: self.question_version,
            model: self.model,
            era: self.era,
            subject_kind: self.subject_kind,
            subject_a: self.subject_a,
            subject_b: self.subject_b,
            rev_a: self.rev_a,
            rev_b: self.rev_b,
            answer: self.answer,
            top: self.top,
            pick: self.pick,
            reason: self.reason,
            outcome_at: self.outcome_at,
            answered_pick: self.answered_pick,
            decided_at: self.decided_at,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ThresholdMove {
    pub question_id: String,
    pub era: String,
    pub act_at: f64,
    pub ask_at: f64,
    pub reason: String,
    pub moved_at: i64,
}

/// What a question's line rests on since it last moved, in one era.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActRecord {
    /// Model acts whose window closed or that were undone, and Unsure rows a person answered.
    pub acted: usize,
    /// Of those, undone acts and corrected answers.
    pub undone: usize,
    /// Ask rows waiting or open now: the band a lower line would act on.
    pub asks_open: usize,
}

/// What the ask band did since the line last moved, and what it holds now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AskRecord {
    pub lapsed_since_move: usize,
    pub answered_since_move: usize,
    pub open: usize,
    pub waiting: usize,
}

// runtime: one column list shared by every read of a whole row; the tails differ.
const SELECT: &str =
    "SELECT id, question_id, question_version, model, era, subject_kind, subject_a,
    subject_b, rev_a, rev_b, answer, top, pick, swapped_agree, band, reason, state, outcome,
    outcome_at, answered_pick, decided_at FROM decisions";

impl Store {
    /// `tail` follows the shared SELECT; `text` binds first, then `int`.
    async fn select_logged(
        &self,
        tail: &str,
        text: &[&str],
        int: Option<i64>,
    ) -> Result<Vec<Logged>> {
        let sql = sqlx::AssertSqlSafe(format!("{SELECT} {tail}"));
        // runtime: the column list is shared by every whole-row read.
        let mut q = sqlx::query_as::<_, Row>(sql);
        for t in text {
            q = q.bind(*t);
        }
        if let Some(i) = int {
            q = q.bind(i);
        }
        q.fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Row::read)
            .collect()
    }

    pub async fn log_decision(&self, d: &NewLogged) -> Result<String> {
        let id = new_id();
        let era = era(&d.model, d.question_version);
        sqlx::query!(
            "INSERT INTO decisions
               (id, question_id, question_version, model, era, subject_kind, subject_a, subject_b,
                rev_a, rev_b, answer, top, pick, swapped_agree, band, reason, state, decided_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            d.question_id,
            d.question_version,
            d.model,
            era,
            d.subject_kind,
            d.subject_a,
            d.subject_b,
            d.rev_a,
            d.rev_b,
            d.answer,
            d.top,
            d.pick,
            d.swapped_agree,
            d.band.as_str(),
            d.reason,
            d.state.as_str(),
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn logged(&self, id: &str) -> Result<Option<Logged>> {
        Ok(self.select_logged("WHERE id = ?", &[id], None).await?.pop())
    }

    /// The newest decision on these subjects in this era, if any.
    pub async fn last_logged(
        &self,
        question_id: &str,
        subject_a: &str,
        subject_b: Option<&str>,
        era: &str,
    ) -> Result<Option<Logged>> {
        // `IS` so that a missing second subject matches a missing one.
        let sql = sqlx::AssertSqlSafe(format!(
            "{SELECT} WHERE question_id = ? AND subject_a = ? AND subject_b IS ? AND era = ?
              ORDER BY decided_at DESC, id DESC LIMIT 1"
        ));
        // runtime: the column list is shared by every whole-row read.
        let row = sqlx::query_as::<_, Row>(sql)
            .bind(question_id)
            .bind(subject_a)
            .bind(subject_b)
            .bind(era)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Row::read).transpose()
    }

    /// False: it was no longer `from`.
    pub async fn set_decision_state(&self, id: &str, from: State, to: State) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE decisions SET state = ? WHERE id = ? AND state = ?",
            to.as_str(),
            id,
            from.as_str()
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn set_decision_outcome(
        &self,
        id: &str,
        outcome: Outcome,
        answered_pick: Option<&str>,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE decisions SET outcome = ?, outcome_at = ?, answered_pick = COALESCE(?, answered_pick)
              WHERE id = ?",
            outcome.as_str(),
            now(),
            answered_pick,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn link_decision_actions(&self, id: &str, action_ids: &[String]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for a in action_ids {
            sqlx::query!(
                "INSERT OR IGNORE INTO decision_actions (decision_id, action_id) VALUES (?, ?)",
                id,
                a
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(tx.commit().await?)
    }

    /// The journal rows a decision wrote.
    pub async fn decision_action_ids(&self, id: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT action_id FROM decision_actions WHERE decision_id = ? ORDER BY action_id",
            id
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn set_decision_reason(&self, id: &str, reason: &str) -> Result<()> {
        sqlx::query!("UPDATE decisions SET reason = ? WHERE id = ?", reason, id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A read the stage that asked added before settling; only probabilities and picks, no text.
    pub async fn set_decision_answer(&self, id: &str, answer: &str) -> Result<()> {
        sqlx::query!("UPDATE decisions SET answer = ? WHERE id = ?", answer, id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A waiting act becomes a waiting ask with `reason`. False: it was not a waiting act.
    pub async fn demote_decision(&self, id: &str, reason: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE decisions SET band = 'ask', reason = ?
              WHERE id = ? AND band = 'act' AND state = 'waiting'",
            reason,
            id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// A waiting ask takes an open row if fewer than `max` are open, in one statement so two
    /// workers cannot both take the last. False: no seat, or it was no longer waiting.
    pub async fn seat_ask(&self, id: &str, max: i64) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE decisions SET state = 'open'
              WHERE id = ? AND band = 'ask' AND state = 'waiting'
                AND (SELECT COUNT(*) FROM decisions WHERE state = 'open') < ?",
            id,
            max
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// An act the stage that asked has settled: the sweep may now claim it.
    pub async fn mark_act_settled(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE decisions SET reason = NULL
              WHERE id = ? AND band = 'act' AND state = 'waiting' AND reason = 'unsettled'",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A settled waiting act to done, for the one worker that acts on it. False: it was not
    /// one (taken, demoted, or not yet settled).
    pub async fn claim_act(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE decisions SET state = 'done'
              WHERE id = ? AND state = 'waiting' AND band = 'act' AND reason IS NOT 'unsettled'",
            id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Acts no stage settled or demoted before `older_than`: the stage died in between.
    pub async fn unsettled_acts(&self, older_than: i64) -> Result<Vec<Logged>> {
        self.select_logged(
            "WHERE band = 'act' AND state = 'waiting' AND reason = 'unsettled' AND decided_at < ?
             ORDER BY decided_at, id LIMIT 200",
            &[],
            Some(older_than),
        )
        .await
    }

    /// An orphaned unsettled act to done with reason `changed`. False: someone settled it.
    pub async fn close_unsettled(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE decisions SET state = 'done', reason = 'changed'
              WHERE id = ? AND state = 'waiting' AND band = 'act' AND reason = 'unsettled'",
            id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Band act, waiting and settled, highest top first.
    pub async fn waiting_acts(&self, limit: i64) -> Result<Vec<Logged>> {
        self.select_logged(
            "WHERE band = 'act' AND state = 'waiting' AND reason IS NOT 'unsettled'
             ORDER BY top DESC, id LIMIT ?",
            &[],
            Some(limit),
        )
        .await
    }

    /// Band ask, waiting for a seat, highest top first.
    pub async fn waiting_asks(&self, limit: i64) -> Result<Vec<Logged>> {
        self.select_logged(
            "WHERE band = 'ask' AND state = 'waiting' ORDER BY top DESC, id LIMIT ?",
            &[],
            Some(limit),
        )
        .await
    }

    pub async fn open_asks(&self, limit: i64) -> Result<Vec<Logged>> {
        self.select_logged(
            "WHERE state = 'open' ORDER BY top DESC, id LIMIT ?",
            &[],
            Some(limit),
        )
        .await
    }

    pub async fn open_ask_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar!("SELECT COUNT(*) FROM decisions WHERE state = 'open'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Asks with no seat yet.
    pub async fn waiting_ask_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM decisions WHERE band = 'ask' AND state = 'waiting'"
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Whether a person answered `reap_worth` "yes" about this note as it is, in any era, and did not
    /// ask again since: a model's pick never buries it.
    pub async fn person_found_valuable(
        &self,
        id: &str,
        superseded_by: Option<&str>,
        rev: i64,
    ) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            r#"SELECT 1 AS "x!: i64" FROM decisions
                WHERE question_id = 'reap_worth' AND subject_a = ? AND subject_b IS ?
                  AND rev_a = ? AND answered_pick = 'yes'
                  AND COALESCE(reason, '') != 'asked_again'
                LIMIT 1"#,
            id,
            superseded_by,
            rev
        )
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    /// Confirmed or corrected with no journal row, newest first; a reason of `asked_again` leaves the list.
    pub async fn answered_without_action(&self, limit: i64) -> Result<Vec<Logged>> {
        self.select_logged(
            "WHERE outcome IN ('confirmed', 'corrected')
               AND reason IS NOT 'asked_again'
               AND NOT EXISTS (SELECT 1 FROM decision_actions WHERE decision_id = decisions.id)
             ORDER BY outcome_at DESC, id DESC LIMIT ?",
            &[],
            Some(limit),
        )
        .await
    }

    /// Open to lapsed, stamped in `outcome_at` (the outcome stays empty) so the tuner can tell
    /// when.
    pub async fn lapse_asks(&self, older_than: i64) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE decisions SET state = 'lapsed', outcome_at = ?
              WHERE state = 'open' AND decided_at < ?",
            now(),
            older_than
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Done act decisions with no outcome whose newest action is older than `older_than`
    /// become kept: the window runs from the act, not from the decision, which may have waited.
    pub async fn close_outcome_windows(&self, older_than: i64) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE decisions SET outcome = 'kept', outcome_at = ?
              WHERE band = 'act' AND state = 'done' AND outcome IS NULL
                AND (SELECT MAX(a.at) FROM decision_actions da
                       JOIN corpus_actions a ON a.id = da.action_id
                      WHERE da.decision_id = decisions.id) < ?",
            now(),
            older_than
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Armed judging only: an after-write check (`subject_kind = 'write'`) and the gate before a
    /// pursuit is written (`'pursuit'`) ride on a writer unit, which the queue already paces.
    pub async fn decided_since(&self, at: i64) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM decisions
              WHERE decided_at >= ? AND subject_kind NOT IN ('write', 'pursuit')",
            at
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Per question: act decisions waiting.
    pub async fn waiting_counts(&self) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query!(
            r#"SELECT question_id AS "question_id!", COUNT(*) AS "n!: i64" FROM decisions
                WHERE band = 'act' AND state = 'waiting' GROUP BY question_id ORDER BY question_id"#
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| (r.question_id, r.n))
        .collect())
    }

    pub async fn thresholds_in_force(
        &self,
        question_id: &str,
        era: &str,
    ) -> Result<Option<Thresholds>> {
        Ok(sqlx::query!(
            "SELECT act_at, ask_at FROM decision_thresholds
              WHERE question_id = ? AND era = ? ORDER BY id DESC LIMIT 1",
            question_id,
            era
        )
        .fetch_optional(&self.pool)
        .await?
        .map(|r| Thresholds {
            act_at: r.act_at,
            ask_at: r.ask_at,
        }))
    }

    /// Both lines, as set by hand or by a test.
    pub async fn move_thresholds(
        &self,
        question_id: &str,
        era: &str,
        t: Thresholds,
        reason: &str,
    ) -> Result<()> {
        self.record_move(question_id, era, t, (true, true), reason)
            .await
    }

    /// The tuner's move from `from` to `to`; each line that changed starts its evidence over.
    pub async fn move_lines(
        &self,
        question_id: &str,
        era: &str,
        from: Thresholds,
        to: Thresholds,
        reason: &str,
    ) -> Result<()> {
        let changed = |a: f64, b: f64| (a - b).abs() > 1e-9;
        let moved = (
            changed(from.act_at, to.act_at),
            changed(from.ask_at, to.ask_at),
        );
        self.record_move(question_id, era, to, moved, reason).await
    }

    async fn record_move(
        &self,
        question_id: &str,
        era: &str,
        t: Thresholds,
        (act_moved, ask_moved): (bool, bool),
        reason: &str,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT INTO decision_thresholds
               (question_id, era, act_at, ask_at, reason, moved_at, act_moved, ask_moved)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            question_id,
            era,
            t.act_at,
            t.ask_at,
            reason,
            now(),
            act_moved,
            ask_moved
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The newest move of a question's line in this era.
    pub async fn last_threshold_move(
        &self,
        question_id: &str,
        era: &str,
    ) -> Result<Option<ThresholdMove>> {
        Ok(sqlx::query_as!(
            ThresholdMove,
            "SELECT question_id, era, act_at, ask_at, reason, moved_at FROM decision_thresholds
              WHERE question_id = ? AND era = ? ORDER BY id DESC LIMIT 1",
            question_id,
            era
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// When the act line (`ask` false) or the ask line last moved, else 0: what was decided before
    /// stands for the line before.
    pub(crate) async fn moved_at(&self, question_id: &str, era: &str, ask: bool) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            r#"SELECT COALESCE(MAX(moved_at), 0) AS "at!: i64" FROM decision_thresholds
                WHERE question_id = ? AND era = ?
                  AND CASE WHEN ? THEN ask_moved ELSE act_moved END = 1"#,
            question_id,
            era,
            ask
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Decided after the act line last moved: model acts kept or undone, and answered Unsure rows (`corrected`
    /// as undone). Not a refused act (no journal row, or `refused`), a check or the pursuit gate.
    /// An undone merge is one decision however many rows it wrote.
    pub async fn act_record(&self, question_id: &str, era: &str) -> Result<ActRecord> {
        let since = self.moved_at(question_id, era, false).await?;
        let r = sqlx::query!(
            r#"SELECT COUNT(*) AS "acted!: i64",
                      COALESCE(SUM(outcome IN ('undone', 'corrected')), 0) AS "undone!: i64"
                 FROM decisions
                WHERE question_id = ? AND era = ? AND decided_at > ?
                  AND subject_kind NOT IN ('write', 'pursuit')
                  AND ((band = 'act' AND state = 'done' AND outcome IN ('kept', 'undone'))
                    OR (band = 'ask' AND state = 'done' AND outcome IN ('confirmed', 'corrected')))"#,
            question_id,
            era,
            since
        )
        .fetch_one(&self.pool)
        .await?;
        let asks_open = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM decisions
                WHERE question_id = ? AND era = ? AND band = 'ask' AND state IN ('waiting', 'open')
                  AND subject_kind NOT IN ('write', 'pursuit')"#,
            question_id,
            era
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(ActRecord {
            acted: r.acted.max(0) as usize,
            undone: r.undone.max(0) as usize,
            asks_open: asks_open.max(0) as usize,
        })
    }

    /// Lapses and answers since the ask line last moved (by `outcome_at`, which a lapse stamps), and the
    /// rows open and waiting now.
    pub async fn ask_record(&self, question_id: &str, era: &str) -> Result<AskRecord> {
        let since = self.moved_at(question_id, era, true).await?;
        let r = sqlx::query!(
            r#"SELECT
                 COALESCE(SUM(state = 'lapsed' AND outcome_at > ?1), 0) AS "lapsed!: i64",
                 COALESCE(SUM(band = 'ask' AND outcome IN ('confirmed', 'corrected')
                              AND outcome_at > ?1), 0) AS "answered!: i64",
                 COALESCE(SUM(state = 'open'), 0) AS "open!: i64",
                 COALESCE(SUM(band = 'ask' AND state = 'waiting'), 0) AS "waiting!: i64"
               FROM decisions
              WHERE question_id = ?2 AND era = ?3 AND subject_kind NOT IN ('write', 'pursuit')"#,
            since,
            question_id,
            era
        )
        .fetch_one(&self.pool)
        .await?;
        let n = |v: i64| v.max(0) as usize;
        Ok(AskRecord {
            lapsed_since_move: n(r.lapsed),
            answered_since_move: n(r.answered),
            open: n(r.open),
            waiting: n(r.waiting),
        })
    }

    pub async fn threshold_moves(&self, limit: i64) -> Result<Vec<ThresholdMove>> {
        Ok(sqlx::query_as!(
            ThresholdMove,
            "SELECT question_id, era, act_at, ask_at, reason, moved_at FROM decision_thresholds
              ORDER BY id DESC LIMIT ?",
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::actions::{Job, Kind, NewAction, UndoneBy};

    fn sample(band: Band, state: State, top: f64) -> NewLogged {
        NewLogged {
            question_id: "same_note".into(),
            question_version: 1,
            model: "m".into(),
            subject_kind: "pair",
            subject_a: "a".into(),
            subject_b: Some("b".into()),
            rev_a: Some(1),
            rev_b: Some(2),
            answer: "{\"yes\":0.9}".into(),
            top,
            pick: "yes".into(),
            swapped_agree: Some(true),
            band,
            reason: None,
            state,
        }
    }

    fn merge_of(subject: &str, survivor: &str) -> NewAction {
        NewAction {
            job: Job::Dedupe,
            kind: Kind::Merge,
            subject_id: subject.into(),
            survivor_id: Some(survivor.into()),
            detail: None,
            evidence: serde_json::json!({}),
            pair_score: Some(0.9),
        }
    }

    /// A decision linked to one merge action per source.
    async fn merged(store: &Store, sources: &[&str]) -> String {
        let id = store
            .log_decision(&sample(Band::Act, State::Done, 0.97))
            .await
            .unwrap();
        let mut ids = vec![];
        for s in sources {
            ids.push(store.record_action(&merge_of(s, "m")).await.unwrap());
        }
        store.link_decision_actions(&id, &ids).await.unwrap();
        id
    }

    async fn age(store: &Store, id: &str, at: i64) {
        // runtime: test-only backdating.
        sqlx::query("UPDATE decisions SET decided_at = ? WHERE id = ?")
            .bind(at)
            .bind(id)
            .execute(&store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_decision_is_read_back_as_logged() {
        let store = Store::memory().await.unwrap();
        let id = store
            .log_decision(&sample(Band::Ask, State::Waiting, 0.8))
            .await
            .unwrap();
        let l = store.logged(&id).await.unwrap().unwrap();
        assert_eq!(l.band, Band::Ask);
        assert_eq!(l.state, State::Waiting);
        assert_eq!(l.era, "m/v1");
        assert_eq!(l.subject_b.as_deref(), Some("b"));
        assert_eq!((l.rev_a, l.rev_b), (Some(1), Some(2)));
        assert_eq!(l.swapped_agree, Some(true));
        assert_eq!(l.outcome, None);
        assert!(store.logged("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_last_decision_on_a_subject_is_found_per_era() {
        let store = Store::memory().await.unwrap();
        let first = store
            .log_decision(&sample(Band::Act, State::Done, 0.9))
            .await
            .unwrap();
        let mut other = sample(Band::Leave, State::Done, 0.5);
        other.model = "n".into();
        let second = store.log_decision(&other).await.unwrap();
        let find = |a: &'static str, e: &'static str| {
            let store = store.clone();
            async move {
                store
                    .last_logged("same_note", a, Some("b"), e)
                    .await
                    .unwrap()
                    .map(|l| l.id)
            }
        };
        assert_eq!(find("a", "m/v1").await, Some(first));
        assert_eq!(find("a", "n/v1").await, Some(second));
        assert_eq!(find("c", "m/v1").await, None);
    }

    #[tokio::test]
    async fn a_state_moves_only_from_the_state_it_was_in() {
        let store = Store::memory().await.unwrap();
        let id = store
            .log_decision(&sample(Band::Act, State::Waiting, 0.9))
            .await
            .unwrap();
        assert!(
            store
                .set_decision_state(&id, State::Waiting, State::Done)
                .await
                .unwrap()
        );
        assert!(
            !store
                .set_decision_state(&id, State::Waiting, State::Done)
                .await
                .unwrap()
        );
        assert_eq!(store.logged(&id).await.unwrap().unwrap().state, State::Done);
    }

    #[tokio::test]
    async fn waiting_acts_come_highest_top_first() {
        let store = Store::memory().await.unwrap();
        let lo = store
            .log_decision(&sample(Band::Act, State::Waiting, 0.91))
            .await
            .unwrap();
        let hi = store
            .log_decision(&sample(Band::Act, State::Waiting, 0.99))
            .await
            .unwrap();
        store
            .log_decision(&sample(Band::Ask, State::Waiting, 0.995))
            .await
            .unwrap();
        store
            .log_decision(&sample(Band::Act, State::Done, 0.999))
            .await
            .unwrap();
        let ids: Vec<_> = store
            .waiting_acts(10)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec![hi, lo]);
        assert_eq!(store.waiting_asks(10).await.unwrap().len(), 1);
        assert_eq!(
            store.waiting_counts().await.unwrap(),
            vec![("same_note".to_string(), 2)]
        );
    }

    #[tokio::test]
    async fn open_rows_older_than_the_lapse_are_lapsed_and_younger_ones_stay() {
        let store = Store::memory().await.unwrap();
        let old = store
            .log_decision(&sample(Band::Ask, State::Open, 0.8))
            .await
            .unwrap();
        let young = store
            .log_decision(&sample(Band::Ask, State::Open, 0.7))
            .await
            .unwrap();
        age(&store, &old, 100).await;
        assert_eq!(store.open_ask_count().await.unwrap(), 2);
        assert_eq!(store.lapse_asks(now() - 50).await.unwrap(), 1);
        assert_eq!(
            store.logged(&old).await.unwrap().unwrap().state,
            State::Lapsed
        );
        assert_eq!(
            store.logged(&young).await.unwrap().unwrap().state,
            State::Open
        );
        assert_eq!(store.open_asks(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn undoing_any_action_of_a_decision_marks_it_undone_once() {
        let store = Store::memory().await.unwrap();
        let id = merged(&store, &["x", "y"]).await;
        store
            .undo_action_on("x", Kind::Merge, UndoneBy::Operator, "no")
            .await
            .unwrap();
        let first = store.logged(&id).await.unwrap().unwrap();
        assert_eq!(first.outcome, Some(Outcome::Undone));
        // runtime: test-only backdating.
        sqlx::query("UPDATE decisions SET outcome_at = 1 WHERE id = ?")
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .undo_action_on("y", Kind::Merge, UndoneBy::Operator, "no")
            .await
            .unwrap();
        assert_eq!(
            store.logged(&id).await.unwrap().unwrap().outcome_at,
            Some(1)
        );
    }

    #[tokio::test]
    async fn undo_actions_under_marks_the_merges_decision() {
        let store = Store::memory().await.unwrap();
        let id = merged(&store, &["x", "y"]).await;
        store
            .undo_actions_under("m", UndoneBy::Evidence, "gone")
            .await
            .unwrap();
        assert_eq!(
            store.logged(&id).await.unwrap().unwrap().outcome,
            Some(Outcome::Undone)
        );
    }

    #[tokio::test]
    async fn an_undo_of_an_action_no_decision_made_touches_nothing() {
        let store = Store::memory().await.unwrap();
        let id = merged(&store, &["x"]).await;
        store.record_action(&merge_of("z", "m2")).await.unwrap();
        store
            .undo_action_on("z", Kind::Merge, UndoneBy::Operator, "no")
            .await
            .unwrap();
        assert_eq!(store.logged(&id).await.unwrap().unwrap().outcome, None);
    }

    #[tokio::test]
    async fn a_window_that_closed_without_an_undo_is_kept() {
        let store = Store::memory().await.unwrap();
        let with = merged(&store, &["x"]).await;
        let bare = store
            .log_decision(&sample(Band::Act, State::Done, 0.9))
            .await
            .unwrap();
        let waiting = store
            .log_decision(&sample(Band::Act, State::Waiting, 0.9))
            .await
            .unwrap();
        assert_eq!(store.close_outcome_windows(now() + 10).await.unwrap(), 1);
        assert_eq!(
            store.logged(&with).await.unwrap().unwrap().outcome,
            Some(Outcome::Kept)
        );
        assert_eq!(store.logged(&bare).await.unwrap().unwrap().outcome, None);
        assert_eq!(store.logged(&waiting).await.unwrap().unwrap().outcome, None);
    }

    #[tokio::test]
    async fn kept_is_counted_from_the_action_not_the_decision() {
        let store = Store::memory().await.unwrap();
        let fresh = merged(&store, &["x"]).await;
        let old = merged(&store, &["y"]).await;
        for id in [&fresh, &old] {
            age(&store, id, now() - 40 * 86_400).await;
        }
        for (subject, days) in [("x", 1), ("y", 31)] {
            // runtime: test-only backdating.
            sqlx::query("UPDATE corpus_actions SET at = ? WHERE subject_id = ?")
                .bind(now() - days * 86_400)
                .bind(subject)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .close_outcome_windows(now() - 30 * 86_400)
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.logged(&fresh).await.unwrap().unwrap().outcome, None);
        assert_eq!(
            store.logged(&old).await.unwrap().unwrap().outcome,
            Some(Outcome::Kept)
        );
    }

    #[tokio::test]
    async fn an_undo_after_kept_still_counts() {
        let store = Store::memory().await.unwrap();
        let id = merged(&store, &["x"]).await;
        store.close_outcome_windows(now() + 10).await.unwrap();
        store
            .undo_action_on("x", Kind::Merge, UndoneBy::Operator, "no")
            .await
            .unwrap();
        assert_eq!(
            store.logged(&id).await.unwrap().unwrap().outcome,
            Some(Outcome::Undone)
        );
    }

    #[tokio::test]
    async fn decisions_are_counted_since_a_time() {
        let store = Store::memory().await.unwrap();
        let old = store
            .log_decision(&sample(Band::Leave, State::Done, 0.4))
            .await
            .unwrap();
        store
            .log_decision(&sample(Band::Leave, State::Done, 0.4))
            .await
            .unwrap();
        age(&store, &old, 100).await;
        assert_eq!(store.decided_since(0).await.unwrap(), 2);
        assert_eq!(store.decided_since(now() - 50).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn the_newest_threshold_move_is_in_force_for_its_era_only() {
        let store = Store::memory().await.unwrap();
        let t = |a, b| Thresholds {
            act_at: a,
            ask_at: b,
        };
        assert_eq!(store.thresholds_in_force("q", "m/v1").await.unwrap(), None);
        store
            .move_thresholds("q", "m/v1", t(0.9, 0.6), "first")
            .await
            .unwrap();
        store
            .move_thresholds("q", "m/v1", t(0.95, 0.7), "second")
            .await
            .unwrap();
        store
            .move_thresholds("q", "n/v1", t(0.8, 0.5), "other")
            .await
            .unwrap();
        assert_eq!(
            store.thresholds_in_force("q", "m/v1").await.unwrap(),
            Some(t(0.95, 0.7))
        );
        assert_eq!(
            store.thresholds_in_force("q", "n/v1").await.unwrap(),
            Some(t(0.8, 0.5))
        );
        let moves = store.threshold_moves(10).await.unwrap();
        assert_eq!(moves.len(), 3);
        assert_eq!(moves[0].reason, "other");
    }

    #[tokio::test]
    async fn answers_that_left_no_journal_row_are_listed_for_asking_again() {
        let store = Store::memory().await.unwrap();
        let bare = store
            .log_decision(&sample(Band::Ask, State::Done, 0.8))
            .await
            .unwrap();
        let acted = store
            .log_decision(&sample(Band::Ask, State::Done, 0.8))
            .await
            .unwrap();
        let again = store
            .log_decision(&sample(Band::Ask, State::Done, 0.8))
            .await
            .unwrap();
        let open = store
            .log_decision(&sample(Band::Ask, State::Open, 0.8))
            .await
            .unwrap();
        store
            .set_decision_outcome(&bare, Outcome::Corrected, Some("no"))
            .await
            .unwrap();
        store
            .set_decision_outcome(&acted, Outcome::Confirmed, Some("yes"))
            .await
            .unwrap();
        store
            .set_decision_outcome(&again, Outcome::Confirmed, Some("yes"))
            .await
            .unwrap();
        store
            .link_decision_actions(&acted, &["act1".into()])
            .await
            .unwrap();
        // runtime: test-only stamp.
        sqlx::query("UPDATE decisions SET reason = 'asked_again' WHERE id = ?")
            .bind(&again)
            .execute(&store.pool)
            .await
            .unwrap();
        let ids: Vec<_> = store
            .answered_without_action(10)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec![bare.clone()]);
        assert!(!ids.contains(&open));
        let l = store.logged(&bare).await.unwrap().unwrap();
        assert_eq!(l.answered_pick.as_deref(), Some("no"));
        assert!(l.outcome_at.is_some());
    }

    /// `n` decisions of `question_id` in era `m/v1`, each with `outcome` once logged.
    async fn logged_n(
        store: &Store,
        question_id: &str,
        kind: &'static str,
        band: Band,
        state: State,
        outcome: Option<Outcome>,
        n: usize,
    ) -> Vec<String> {
        let mut ids = vec![];
        for _ in 0..n {
            let mut d = sample(band, state, 0.95);
            d.question_id = question_id.into();
            d.subject_kind = kind;
            let id = store.log_decision(&d).await.unwrap();
            if let Some(o) = outcome {
                store.set_decision_outcome(&id, o, None).await.unwrap();
            }
            ids.push(id);
        }
        ids
    }

    #[tokio::test]
    async fn an_undone_merge_counts_against_pair_relation_once_however_many_rows_it_wrote() {
        let store = Store::memory().await.unwrap();
        let id = merged(&store, &["x", "y", "z"]).await;
        // runtime: test-only relabel.
        sqlx::query("UPDATE decisions SET question_id = 'pair_relation' WHERE id = ?")
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        for s in ["x", "y", "z"] {
            store
                .undo_action_on(s, Kind::Merge, UndoneBy::Operator, "no")
                .await
                .unwrap();
        }
        assert_eq!(
            store.act_record("pair_relation", "m/v1").await.unwrap(),
            ActRecord {
                acted: 1,
                undone: 1,
                asks_open: 0
            }
        );
    }

    #[tokio::test]
    async fn a_decision_whose_window_is_still_open_does_not_count_as_kept() {
        let store = Store::memory().await.unwrap();
        let open = merged(&store, &["x"]).await;
        merged(&store, &["y"]).await;
        // runtime: test-only backdating.
        sqlx::query("UPDATE corpus_actions SET at = ? WHERE subject_id = 'y'")
            .bind(now() - 31 * 86_400)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .close_outcome_windows(now() - 30 * 86_400)
            .await
            .unwrap();
        assert_eq!(store.logged(&open).await.unwrap().unwrap().outcome, None);
        assert_eq!(
            store.act_record("same_note", "m/v1").await.unwrap(),
            ActRecord {
                acted: 1,
                undone: 0,
                asks_open: 0
            }
        );
    }

    #[tokio::test]
    async fn a_persons_answers_count_and_refusals_checks_and_gates_do_not() {
        let store = Store::memory().await.unwrap();
        let q = "same_note";
        logged_n(
            &store,
            q,
            "pair",
            Band::Act,
            State::Done,
            Some(Outcome::Kept),
            2,
        )
        .await;
        logged_n(
            &store,
            q,
            "pair",
            Band::Act,
            State::Done,
            Some(Outcome::Undone),
            1,
        )
        .await;
        logged_n(
            &store,
            q,
            "pair",
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            3,
        )
        .await;
        logged_n(
            &store,
            q,
            "pair",
            Band::Ask,
            State::Done,
            Some(Outcome::Corrected),
            1,
        )
        .await;
        // Refused at apply time: done, act, `changed`, nothing linked.
        for id in logged_n(&store, q, "pair", Band::Act, State::Done, None, 2).await {
            store.set_decision_reason(&id, "changed").await.unwrap();
        }
        logged_n(
            &store,
            q,
            "pair",
            Band::Act,
            State::Done,
            Some(Outcome::Refused),
            2,
        )
        .await;
        logged_n(
            &store,
            q,
            "write",
            Band::Act,
            State::Done,
            Some(Outcome::Kept),
            2,
        )
        .await;
        logged_n(
            &store,
            q,
            "pursuit",
            Band::Ask,
            State::Done,
            Some(Outcome::Corrected),
            2,
        )
        .await;
        // The ask band now: waiting and open count, a lapsed one does not.
        logged_n(&store, q, "pair", Band::Ask, State::Waiting, None, 2).await;
        logged_n(&store, q, "pair", Band::Ask, State::Open, None, 1).await;
        logged_n(&store, q, "pair", Band::Ask, State::Lapsed, None, 1).await;
        assert_eq!(
            store.act_record(q, "m/v1").await.unwrap(),
            ActRecord {
                acted: 7,
                undone: 2,
                asks_open: 3
            }
        );
        assert_eq!(
            store.act_record(q, "n/v1").await.unwrap(),
            ActRecord::default()
        );
    }

    #[tokio::test]
    async fn only_what_was_decided_after_the_last_move_counts() {
        let store = Store::memory().await.unwrap();
        let q = "same_note";
        let before = logged_n(
            &store,
            q,
            "pair",
            Band::Act,
            State::Done,
            Some(Outcome::Kept),
            2,
        )
        .await;
        for id in &before {
            age(&store, id, now() - 100).await;
        }
        let t = Thresholds {
            act_at: 0.92,
            ask_at: 0.6,
        };
        store.move_thresholds(q, "m/v1", t, "moved").await.unwrap();
        // runtime: test-only backdating.
        sqlx::query("UPDATE decision_thresholds SET moved_at = ?")
            .bind(now() - 50)
            .execute(&store.pool)
            .await
            .unwrap();
        logged_n(
            &store,
            q,
            "pair",
            Band::Act,
            State::Done,
            Some(Outcome::Undone),
            1,
        )
        .await;
        assert_eq!(store.act_record(q, "m/v1").await.unwrap().acted, 1);
        assert_eq!(store.act_record(q, "m/v1").await.unwrap().undone, 1);
    }

    #[tokio::test]
    async fn the_ask_record_counts_lapses_and_answers_since_the_last_move() {
        let store = Store::memory().await.unwrap();
        let q = "same_note";
        let old = logged_n(&store, q, "pair", Band::Ask, State::Open, None, 2).await;
        for id in &old {
            age(&store, id, now() - 100).await;
        }
        assert_eq!(store.lapse_asks(now() - 50).await.unwrap(), 2);
        logged_n(
            &store,
            q,
            "pair",
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            3,
        )
        .await;
        logged_n(&store, q, "pair", Band::Ask, State::Open, None, 1).await;
        logged_n(&store, q, "pair", Band::Ask, State::Waiting, None, 4).await;
        assert_eq!(
            store.ask_record(q, "m/v1").await.unwrap(),
            AskRecord {
                lapsed_since_move: 2,
                answered_since_move: 3,
                open: 1,
                waiting: 4
            }
        );
        // A move in the same second stands for everything before it.
        let t = Thresholds {
            act_at: 0.9,
            ask_at: 0.62,
        };
        store.move_thresholds(q, "m/v1", t, "moved").await.unwrap();
        // runtime: test-only stamp, after every row above.
        sqlx::query("UPDATE decision_thresholds SET moved_at = ?")
            .bind(now() + 1)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.ask_record(q, "m/v1").await.unwrap(),
            AskRecord {
                lapsed_since_move: 0,
                answered_since_move: 0,
                open: 1,
                waiting: 4
            }
        );
    }

    #[tokio::test]
    async fn the_act_line_keeps_its_evidence_across_an_ask_only_move() {
        let store = Store::memory().await.unwrap();
        let q = "same_note";
        let t = |act_at, ask_at| Thresholds { act_at, ask_at };
        let early = logged_n(
            &store,
            q,
            "pair",
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            25,
        )
        .await;
        for id in &early {
            age(&store, id, now() - 100).await;
            // runtime: test-only backdating.
            sqlx::query("UPDATE decisions SET outcome_at = ? WHERE id = ?")
                .bind(now() - 100)
                .bind(id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        store
            .move_lines(
                q,
                "m/v1",
                t(0.9, 0.6),
                t(0.9, 0.62),
                "a row lapsed unanswered",
            )
            .await
            .unwrap();
        // runtime: test-only backdating.
        sqlx::query("UPDATE decision_thresholds SET moved_at = ?")
            .bind(now() - 50)
            .execute(&store.pool)
            .await
            .unwrap();
        logged_n(
            &store,
            q,
            "pair",
            Band::Ask,
            State::Done,
            Some(Outcome::Confirmed),
            5,
        )
        .await;
        assert_eq!(store.act_record(q, "m/v1").await.unwrap().acted, 30);
        assert_eq!(
            store
                .ask_record(q, "m/v1")
                .await
                .unwrap()
                .answered_since_move,
            5
        );
        // An act-only move starts the act line over and leaves the ask line's evidence.
        store
            .move_lines(q, "m/v1", t(0.9, 0.62), t(0.88, 0.62), "moved")
            .await
            .unwrap();
        assert_eq!(store.act_record(q, "m/v1").await.unwrap().acted, 0);
        assert_eq!(
            store
                .ask_record(q, "m/v1")
                .await
                .unwrap()
                .answered_since_move,
            5
        );
        assert_eq!(
            store.thresholds_in_force(q, "m/v1").await.unwrap(),
            Some(t(0.88, 0.62))
        );
    }
}
