//! The corpus journal: what the base did to itself, and what it took back.
//!
//! Every action a corpus job takes on its own writes one row per subject:
//! what was hidden, buried or created, in favour of what, on what evidence.
//! An undo — a person's button, or the base on what use showed — stamps the
//! row rather than deleting it. The record the ranking side has in
//! `generations`, for the corpus: "why is this hidden" and "what did the base
//! do to itself" always have an answer.
//!
//! The rows are also the memory. An action taken back on a subject is not
//! taken again on that subject by that job, and the action sites read this
//! before they act — the corpus side's `tried_candidates`.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

/// Which job acted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Dedupe,
    Reap,
    Promote,
    Judgement,
}

impl Job {
    pub fn as_str(&self) -> &'static str {
        match self {
            Job::Dedupe => "dedupe",
            Job::Reap => "reap",
            Job::Promote => "promote",
            Job::Judgement => "judgement",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dedupe" => Some(Job::Dedupe),
            "reap" => Some(Job::Reap),
            "promote" => Some(Job::Promote),
            "judgement" => Some(Job::Judgement),
            _ => None,
        }
    }
}

/// What was done to the subject.
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
            _ => None,
        }
    }
}

/// Who took an action back.
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
    /// The pair's cosine, for the dedupe kinds; read by the review
    /// threshold's bands.
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

/// Write one row through whatever executor the caller has, so a site that
/// acts inside a transaction can journal inside the same one.
pub(crate) async fn insert<'e, E>(ex: E, a: &NewAction) -> Result<String>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let id = new_id();
    let evidence = serde_json::to_string(&a.evidence)
        .map_err(|e| Error::Store(format!("corpus_actions: {e}")))?;
    sqlx::query(
        "INSERT INTO corpus_actions
           (id, at, job, kind, subject_id, survivor_id, detail, evidence_json, pair_score)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(now())
    .bind(a.job.as_str())
    .bind(a.kind.as_str())
    .bind(&a.subject_id)
    .bind(&a.survivor_id)
    .bind(&a.detail)
    .bind(evidence)
    .bind(a.pair_score)
    .execute(ex)
    .await?;
    Ok(id)
}

impl Store {
    pub async fn record_action(&self, a: &NewAction) -> Result<String> {
        insert(&self.pool, a).await
    }

    /// Open rows — not taken back — of these kinds, oldest first, at most
    /// `limit` in all.
    pub async fn open_actions(&self, kinds: &[Kind], limit: usize) -> Result<Vec<Action>> {
        self.open_actions_after(kinds, &super::Cursor::default(), limit)
            .await
    }

    /// The same, from a position rather than from the beginning.
    ///
    /// A reader with a limit needs this: an open row leaves the set only by
    /// being taken back, which is the rare case, so a reader that always
    /// starts at the oldest row sees the same `limit` rows on every pass and
    /// never reaches anything written since. Rule 1 walks forward on this
    /// cursor and wraps.
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

    /// The open row on this subject of this kind, if any.
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

    /// Whether an action of this kind on this subject was ever taken back —
    /// the memory an action site reads before acting again.
    pub async fn action_was_undone(&self, subject_id: &str, kind: Kind) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM corpus_actions
              WHERE subject_id = ? AND kind = ? AND undone_at IS NOT NULL LIMIT 1",
        )
        .bind(subject_id)
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    /// Stamp the open rows on `subject_id` of `kind`. Rows stamped.
    pub async fn undo_action_on(
        &self,
        subject_id: &str,
        kind: Kind,
        by: UndoneBy,
        reason: &str,
    ) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ?
              WHERE subject_id = ? AND kind = ? AND undone_at IS NULL",
        )
        .bind(now())
        .bind(by.as_str())
        .bind(reason)
        .bind(subject_id)
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Stamp every open *merge* row whose survivor is `survivor_id`: a merge's
    /// originals, taken back together.
    ///
    /// Only the merge rows. A merge is an ordinary artifact once it exists, so
    /// it can later win a supersession — and that row names it as `survivor_id`
    /// too. Stamping it here would report an undo that never happened, hide the
    /// still-superseded loser from both retract rules (`open_action_on` returns
    /// nothing for a stamped row), and make dedupe refuse it forever as
    /// `TAKEN_BACK`.
    pub async fn undo_actions_under(
        &self,
        survivor_id: &str,
        by: UndoneBy,
        reason: &str,
    ) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE corpus_actions SET undone_at = ?, undone_by = ?, undone_reason = ?
              WHERE survivor_id = ? AND kind = 'merge' AND undone_at IS NULL",
        )
        .bind(now())
        .bind(by.as_str())
        .bind(reason)
        .bind(survivor_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Newest first, for disclosure.
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
        // `m` exists now, so it can win a supersession of its own.
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
        // All three in the same second, which is the whole clock: the id has
        // to break the tie or the second row is never reached.
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
