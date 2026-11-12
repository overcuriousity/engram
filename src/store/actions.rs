//! The corpus journal: one row per subject a corpus job acted on. An undo stamps the row;
//! an action taken back on a subject is not taken again by that job.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Dedupe,
    Reap,
    Promote,
    Judgement,
    /// The idle pass's own corpus rules: condense.
    Sleep,
}

impl Job {
    pub fn as_str(&self) -> &'static str {
        match self {
            Job::Dedupe => "dedupe",
            Job::Reap => "reap",
            Job::Promote => "promote",
            Job::Judgement => "judgement",
            Job::Sleep => "sleep",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dedupe" => Some(Job::Dedupe),
            "reap" => Some(Job::Reap),
            "promote" => Some(Job::Promote),
            "judgement" => Some(Job::Judgement),
            "sleep" => Some(Job::Sleep),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An original folded into a merge; `survivor_id` is the merge.
    Merge,
    /// Hidden in favour of another artifact; `survivor_id` is the winner.
    Supersede,
    /// Deprecated on the judge's word that it held nothing.
    Discard,
    /// Buried: text in the graveyard, point deleted.
    Reap,
    /// A window promoted to an artifact; the subject is `corpus_id#idx`.
    Promote,
    /// A reminder or event filed from a reading; the subject is the moment.
    Moment,
    /// Subject and survivor are the same; `evidence_json.version` names the version retired.
    Condense,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Merge => "merge",
            Kind::Supersede => "supersede",
            Kind::Discard => "discard",
            Kind::Reap => "reap",
            Kind::Promote => "promote",
            Kind::Moment => "moment",
            Kind::Condense => "condense",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(Kind::Merge),
            "supersede" => Some(Kind::Supersede),
            "discard" => Some(Kind::Discard),
            "reap" => Some(Kind::Reap),
            "promote" => Some(Kind::Promote),
            "moment" => Some(Kind::Moment),
            "condense" => Some(Kind::Condense),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoneBy {
    /// A person pressed something.
    Operator,
    /// The base, on what use showed.
    Evidence,
}

impl UndoneBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            UndoneBy::Operator => "operator",
            UndoneBy::Evidence => "evidence",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(UndoneBy::Operator),
            "evidence" => Some(UndoneBy::Evidence),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewAction {
    pub job: Job,
    pub kind: Kind,
    pub subject_id: String,
    pub survivor_id: Option<String>,
    pub detail: Option<String>,
    pub evidence: serde_json::Value,
    /// The pair's cosine, for the dedupe kinds; read by the review threshold's bands.
    pub pair_score: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub at: i64,
    pub job: Job,
    pub kind: Kind,
    pub subject_id: String,
    pub survivor_id: Option<String>,
    pub detail: Option<String>,
    pub evidence_json: String,
    pub pair_score: Option<f32>,
    pub undone_at: Option<i64>,
    pub undone_by: Option<UndoneBy>,
    pub undone_reason: Option<String>,
}

const COLUMNS: &str = "id, at, job, kind, subject_id, survivor_id, detail, evidence_json, \
                       pair_score, undone_at, undone_by, undone_reason";

fn read(r: &sqlx::sqlite::SqliteRow) -> Result<Action> {
    let job: String = r.get("job");
    let kind: String = r.get("kind");
    Ok(Action {
        id: r.get("id"),
        at: r.get("at"),
        job: Job::parse(&job).ok_or_else(|| Error::Store(format!("corpus_actions: job {job}")))?,
        kind: Kind::parse(&kind)
            .ok_or_else(|| Error::Store(format!("corpus_actions: kind {kind}")))?,
        subject_id: r.get("subject_id"),
        survivor_id: r.get("survivor_id"),
        detail: r.get("detail"),
        evidence_json: r.get("evidence_json"),
        pair_score: r.get("pair_score"),
        undone_at: r.get("undone_at"),
        undone_by: r
            .get::<Option<String>, _>("undone_by")
            .as_deref()
            .and_then(UndoneBy::parse),
        undone_reason: r.get("undone_reason"),
    })
}

/// Takes any executor, so a site inside a transaction can journal in the same one.
pub(crate) async fn insert<'e, E>(ex: E, a: &NewAction) -> Result<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let id = new_id();
    let evidence = serde_json::to_string(&a.evidence)
        .map_err(|e| Error::Store(format!("corpus_actions: {e}")))?;
    sqlx::query!(
        "INSERT INTO corpus_actions
           (id, at, job, kind, subject_id, survivor_id, detail, evidence_json, pair_score)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &id,
        now(),
        a.job.as_str(),
        a.kind.as_str(),
        &a.subject_id,
        &a.survivor_id,
        &a.detail,
        evidence,
        a.pair_score
    )
    .execute(ex)
    .await?;
    Ok(id)
}

impl Store {
    pub async fn record_action(&self, a: &NewAction) -> Result<String> {
        insert(&self.pool, a).await
    }

    pub async fn open_actions(&self, kinds: &[Kind], limit: usize) -> Result<Vec<Action>> {
        self.open_actions_after(kinds, &super::Cursor::default(), limit)
            .await
    }

    /// Open rows rarely leave the set: a limited reader must page forward to reach new rows.
    pub async fn open_actions_after(
        &self,
        kinds: &[Kind],
        after: &super::Cursor,
        limit: usize,
    ) -> Result<Vec<Action>> {
        let mut out = Vec::new();
        for kind in kinds {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {COLUMNS} FROM corpus_actions
                  WHERE kind = ? AND undone_at IS NULL
                    AND (at > ? OR (at = ? AND id > ?))
                  ORDER BY at ASC, id ASC LIMIT ?"
            )))
            .bind(kind.as_str())
            .bind(after.at)
            .bind(after.at)
            .bind(&after.id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
            for r in &rows {
                out.push(read(r)?);
            }
        }
        out.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        out.truncate(limit);
        Ok(out)
    }

    pub async fn action(&self, id: &str) -> Result<Option<Action>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM corpus_actions WHERE id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(read)
        .transpose()
    }

    pub async fn open_action_on(&self, subject_id: &str, kind: Kind) -> Result<Option<Action>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM corpus_actions
              WHERE subject_id = ? AND kind = ? AND undone_at IS NULL
              ORDER BY at DESC, id DESC LIMIT 1"
        )))
        .bind(subject_id)
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(read)
        .transpose()
    }

    /// Undone or not. Counted per job: a shared count let one job's actions spend another's budget.
    pub async fn actions_since(&self, job: Job, at: i64) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM corpus_actions
              WHERE at > ? AND job = ? AND kind != 'moment'",
            at,
            job.as_str()
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Whether an action of this kind on this subject was ever taken back —
    /// the memory an action site reads before acting again.
    pub async fn action_was_undone(&self, subject_id: &str, kind: Kind) -> Result<bool> {
        Ok(sqlx::query_scalar!(
            "SELECT 1 FROM corpus_actions
              WHERE subject_id = ? AND kind = ? AND undone_at IS NOT NULL LIMIT 1",
            subject_id,
            kind.as_str()
        )
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    pub async fn undo_action_on(
        &self,
        subject_id: &str,
        kind: Kind,
        by: UndoneBy,
        reason: &str,
    ) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ?
              WHERE subject_id = ? AND kind = ? AND undone_at IS NULL",
            now(),
            by.as_str(),
            reason,
            subject_id,
            kind.as_str()
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Only merge rows: a merge can later win a supersession, and stamping that row would report
    /// an undo that never happened and make dedupe refuse the loser forever.
    pub async fn undo_actions_under(
        &self,
        survivor_id: &str,
        by: UndoneBy,
        reason: &str,
    ) -> Result<u64> {
        Ok(sqlx::query!(
            "UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ?
              WHERE survivor_id = ? AND kind = 'merge' AND undone_at IS NULL",
            now(),
            by.as_str(),
            reason,
            survivor_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn recent_actions(&self, limit: usize) -> Result<Vec<Action>> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM corpus_actions ORDER BY at DESC, id DESC LIMIT ?"
        )))
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(read)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_job_s_budget_counts_its_own_writes_in_the_window_undone_or_not() {
        let store = Store::memory().await.unwrap();
        store.record_action(&condense_of("a")).await.unwrap();
        store.record_action(&condense_of("b")).await.unwrap();
        store
            .undo_action_on("a", Kind::Condense, UndoneBy::Evidence, "back")
            .await
            .unwrap();
        assert_eq!(store.actions_since(Job::Sleep, 0).await.unwrap(), 2);
        assert_eq!(store.actions_since(Job::Sleep, i64::MAX).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn one_job_s_writes_never_spend_another_job_s_week() {
        let store = Store::memory().await.unwrap();
        store.record_action(&condense_of("a")).await.unwrap();
        for (i, (job, kind)) in [
            (Job::Dedupe, Kind::Merge),
            (Job::Dedupe, Kind::Supersede),
            (Job::Judgement, Kind::Discard),
            (Job::Reap, Kind::Reap),
            (Job::Promote, Kind::Promote),
            // A reminder read out of a note is exempt in every job: it rearranges nothing.
            (Job::Judgement, Kind::Moment),
        ]
        .into_iter()
        .enumerate()
        {
            store
                .record_action(&NewAction {
                    job,
                    kind,
                    subject_id: format!("other-{i}"),
                    survivor_id: None,
                    detail: None,
                    evidence: serde_json::json!({}),
                    pair_score: None,
                })
                .await
                .unwrap();
        }
        for (job, expected) in [
            (Job::Sleep, 1),
            (Job::Dedupe, 2),
            (Job::Reap, 1),
            (Job::Promote, 1),
            (Job::Judgement, 1),
        ] {
            assert_eq!(
                store.actions_since(job, 0).await.unwrap(),
                expected,
                "{job:?} was charged for somebody else's work"
            );
        }
    }

    fn condense_of(subject: &str) -> NewAction {
        NewAction {
            job: Job::Sleep,
            kind: Kind::Condense,
            subject_id: subject.into(),
            survivor_id: Some(subject.into()),
            detail: Some("version 1 retired".into()),
            evidence: serde_json::json!({ "version": 1 }),
            pair_score: None,
        }
    }

    fn merge_of(subject: &str, survivor: &str) -> NewAction {
        NewAction {
            job: Job::Dedupe,
            kind: Kind::Merge,
            subject_id: subject.into(),
            survivor_id: Some(survivor.into()),
            detail: Some("the same note twice".into()),
            evidence: serde_json::json!({ "pair_id": 7 }),
            pair_score: Some(0.91),
        }
    }

    #[tokio::test]
    async fn a_recorded_action_is_open_until_it_is_taken_back_and_then_remembered() {
        let store = Store::memory().await.unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        store.record_action(&merge_of("b", "m")).await.unwrap();
        assert_eq!(
            store.open_actions(&[Kind::Merge], 10).await.unwrap().len(),
            2
        );
        assert!(
            store
                .open_action_on("a", Kind::Merge)
                .await
                .unwrap()
                .is_some()
        );
        assert!(!store.action_was_undone("a", Kind::Merge).await.unwrap());

        assert_eq!(
            store
                .undo_actions_under("m", UndoneBy::Evidence, "the survivor was not found")
                .await
                .unwrap(),
            2
        );
        assert!(
            store
                .open_actions(&[Kind::Merge], 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.action_was_undone("a", Kind::Merge).await.unwrap());
        let a = store.recent_actions(10).await.unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].undone_by, Some(UndoneBy::Evidence));
        assert_eq!(
            a[0].undone_reason.as_deref(),
            Some("the survivor was not found")
        );
        assert_eq!(a[0].pair_score, Some(0.91));
        assert_eq!(a[0].evidence_json, r#"{"pair_id":7}"#);
    }

    #[tokio::test]
    async fn an_undo_stamps_only_the_open_rows_of_its_kind_on_its_subject() {
        let store = Store::memory().await.unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        store
            .record_action(&NewAction {
                kind: Kind::Discard,
                survivor_id: None,
                ..merge_of("a", "m")
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .undo_action_on("a", Kind::Discard, UndoneBy::Operator, "button")
                .await
                .unwrap(),
            1
        );
        assert!(
            store
                .open_action_on("a", Kind::Merge)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .undo_action_on("a", Kind::Discard, UndoneBy::Operator, "again")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn taking_a_merge_back_leaves_a_supersession_the_merge_won() {
        let store = Store::memory().await.unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        store
            .record_action(&NewAction {
                kind: Kind::Supersede,
                ..merge_of("x", "m")
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .undo_actions_under("m", UndoneBy::Evidence, "the survivor was not found")
                .await
                .unwrap(),
            1,
            "only the merge row"
        );
        assert!(
            store
                .open_action_on("x", Kind::Supersede)
                .await
                .unwrap()
                .is_some(),
            "x is still superseded, so its row is still open"
        );
        assert!(!store.action_was_undone("x", Kind::Supersede).await.unwrap());
    }

    #[tokio::test]
    async fn open_actions_walks_every_kind_asked_for_oldest_first() {
        let store = Store::memory().await.unwrap();
        store
            .record_action(&NewAction {
                kind: Kind::Supersede,
                ..merge_of("s", "w")
            })
            .await
            .unwrap();
        store.record_action(&merge_of("a", "m")).await.unwrap();
        let all = store
            .open_actions(&[Kind::Merge, Kind::Supersede], 10)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, Kind::Supersede, "written first");
        assert_eq!(
            store
                .open_actions(&[Kind::Merge, Kind::Supersede], 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_reader_with_a_limit_can_walk_past_the_rows_it_has_seen() {
        let store = Store::memory().await.unwrap();
        for subject in ["a", "b", "c"] {
            store.record_action(&merge_of(subject, "m")).await.unwrap();
        }
        // Same second for all three: the id must break the tie or the second row is never reached.
        let first = store.open_actions(&[Kind::Merge], 1).await.unwrap();
        let at = super::super::Cursor {
            at: first[0].at,
            id: first[0].id.clone(),
        };
        let rest = store
            .open_actions_after(&[Kind::Merge], &at, 10)
            .await
            .unwrap();
        assert_eq!(rest.len(), 2);
        assert!(rest.iter().all(|a| a.id != first[0].id));
        let end = super::super::Cursor {
            at: rest[1].at,
            id: rest[1].id.clone(),
        };
        assert!(
            store
                .open_actions_after(&[Kind::Merge], &end, 10)
                .await
                .unwrap()
                .is_empty(),
            "the end of a lap: the caller starts over from the default"
        );
    }

    #[test]
    fn the_names_round_trip() {
        for k in [
            Kind::Merge,
            Kind::Supersede,
            Kind::Discard,
            Kind::Reap,
            Kind::Promote,
            Kind::Moment,
        ] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        for j in [Job::Dedupe, Job::Reap, Job::Promote, Job::Judgement] {
            assert_eq!(Job::parse(j.as_str()), Some(j));
        }
        assert_eq!(UndoneBy::parse("evidence"), Some(UndoneBy::Evidence));
        assert_eq!(
            crate::store::pairs::DecidedBy::parse("evidence"),
            Some(crate::store::pairs::DecidedBy::Evidence)
        );
    }
}
