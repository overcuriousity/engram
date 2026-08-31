//! A time attached to an artifact. See the schema comment and the spec's §3.

use crate::error::Result;
use crate::store::{new_id, Store};
use sqlx::Row;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Due,
    Event,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Due => "due",
            Kind::Event => "event",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "due" => Some(Kind::Due),
            "event" => Some(Kind::Event),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Set,
    Cue,
    Classified,
    Extracted,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Set => "set",
            Source::Cue => "cue",
            Source::Classified => "classified",
            Source::Extracted => "extracted",
        }
    }
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "set" => Some(Source::Set),
            "cue" => Some(Source::Cue),
            "classified" => Some(Source::Classified),
            "extracted" => Some(Source::Extracted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Moment {
    pub id: String,
    pub artifact_id: String,
    pub kind: Kind,
    pub at: Option<i64>,
    pub until: Option<i64>,
    pub tz: String,
    pub rule: Option<String>,
    pub source: Source,
    pub span: Option<String>,
    pub done_at: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub notified_at: Option<i64>,
    pub created_at: i64,
}

pub struct NewMoment {
    pub artifact_id: String,
    pub kind: Kind,
    pub at: Option<i64>,
    pub tz: String,
    pub rule: Option<String>,
    pub source: Source,
    pub span: Option<String>,
}

/// A moment with the artifact it hangs on: its title, or the opening line
/// where there is none.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DueRow {
    pub moment: Moment,
    pub title: String,
    pub opening: String,
}

fn moment_of(r: &sqlx::sqlite::SqliteRow) -> Moment {
    Moment {
        id: r.get("id"),
        artifact_id: r.get("artifact_id"),
        kind: Kind::parse(r.get::<String, _>("kind").as_str()).unwrap_or(Kind::Event),
        at: r.get("at"),
        until: r.get("until"),
        tz: r.get("tz"),
        rule: r.get("rule"),
        source: Source::parse(r.get::<String, _>("source").as_str()).unwrap_or(Source::Extracted),
        span: r.get("span"),
        done_at: r.get("done_at"),
        snoozed_until: r.get("snoozed_until"),
        notified_at: r.get("notified_at"),
        created_at: r.get("created_at"),
    }
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> DueRow {
    let text: String = r.get("text");
    let opening = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect::<String>();
    let title: Option<String> = r.get("title");
    DueRow { moment: moment_of(r), title: title.filter(|t| !t.is_empty()).unwrap_or_else(|| opening.clone()), opening }
}

const JOINED: &str = "SELECT m.*, a.title, a.text FROM moments m JOIN artifacts a ON a.id = m.artifact_id";

/// What a push is owed: due, dated, undone, unnotified. A snoozed row is owed
/// once its snooze has elapsed — `snoozed_until` is what it is due *at* from
/// then on, which is why the reads below coalesce it over `at`.
const OWED: &str = "m.kind = 'due' AND m.done_at IS NULL AND m.notified_at IS NULL AND m.at IS NOT NULL";

impl Store {
    /// A moment hangs off an artifact; the note is the artifact's corpus.
    pub async fn corpus_of_moment(&self, moment_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT a.corpus_id FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE m.id = ?",
        )
        .bind(moment_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Any reminder still open on any artifact of this note. `complete_moment`
    /// arms the next occurrence of a recurring reminder before this is asked,
    /// so a recurring done answers `true` and retires nothing.
    ///
    /// `kind = 'due'` and not every kind: an event is a date the note mentions,
    /// which nobody completes and which would therefore hold a note open
    /// forever. Retirement is about reminders.
    pub async fn has_open_reminder_for_corpus(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE a.corpus_id = ? AND m.kind = 'due' AND m.done_at IS NULL",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Was this note ever *read* as a reminder, as opposed to given a date by
    /// a person? `source` already records exactly that: `cue` and `classified`
    /// are the two readings, `set` is a person, `extracted` is a date
    /// mentioned in passing prose. Every moment the note ever had is
    /// considered, because moving a cue reminder's date writes a fresh `set`
    /// row and the note does not stop having been a reminder.
    pub async fn corpus_was_read_as_reminder(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE a.corpus_id = ? AND m.source IN ('cue', 'classified')",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    pub async fn insert_moment(&self, m: &NewMoment) -> Result<String> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, rule, source, span, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&m.artifact_id)
        .bind(m.kind.as_str())
        .bind(m.at)
        .bind(&m.tz)
        .bind(&m.rule)
        .bind(m.source.as_str())
        .bind(&m.span)
        .bind(crate::store::now())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Does this artifact already carry a moment of this kind at this
    /// instant? The moments stage asks before it re-inserts what it just read,
    /// so a row `delete_read_moments` kept — one already done, pushed or
    /// snoozed — is not doubled by the reading that would have made it again.
    pub async fn has_moment_at(&self, artifact_id: &str, kind: Kind, at: i64) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments WHERE artifact_id = ? AND kind = ? AND at = ?",
        )
        .bind(artifact_id)
        .bind(kind.as_str())
        .bind(at)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// How many occurrences of one recurrence have existed on this artifact,
    /// done rows included — the history of a recurring reminder is its rows,
    /// so counting them is counting the occurrences.
    pub async fn occurrences_of_rule(&self, artifact_id: &str, rule: &str) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM moments WHERE artifact_id = ? AND kind = 'due' AND rule = ?")
            .bind(artifact_id)
            .bind(rule)
            .fetch_one(&self.pool)
            .await?)
    }

    /// Has a `COUNT=n` rule had its n occurrences? False for a rule with no
    /// COUNT, which is open-ended, and for one that does not parse — an
    /// unreadable rule is `next_after`'s to refuse, not this read's.
    pub async fn rule_is_exhausted(&self, artifact_id: &str, rule: &str) -> Result<bool> {
        let Some(count) = crate::core::moments::rule_count(rule) else { return Ok(false) };
        Ok(self.occurrences_of_rule(artifact_id, rule).await? >= count as i64)
    }

    pub async fn moment(&self, id: &str) -> Result<Option<Moment>> {
        Ok(sqlx::query("SELECT * FROM moments WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| moment_of(&r)))
    }

    /// What the stage read last time, so a re-read replaces rather than
    /// duplicates. A row somebody set is not the stage's to delete — and
    /// neither is one that has since been acted on. Every embed re-arms this
    /// stage, so without the second half a reindex or a switched embed model
    /// would delete a reminder finished months ago and read it back fresh:
    /// it would return to the band and push again. `done_at`, `notified_at`
    /// and `snoozed_until` are the three marks that say a row has a history,
    /// and a row with a history outlives the reading that made it.
    pub async fn delete_read_moments(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND source != 'set'
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL",
        )
            .bind(artifact_id)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Open reminders: undone, on an active artifact, and either undated or
    /// due before `to` and not snoozed past `now`. Dated first, by time.
    pub async fn open_due(&self, now: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active'
               AND (m.at IS NULL OR (m.at < ? AND (m.snoozed_until IS NULL OR m.snoozed_until <= ?)))
             ORDER BY m.at IS NULL, m.at, m.created_at"
        )))
        .bind(to)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    /// The search lift's read: for these artifacts, the earliest open due
    /// moment before `to`, snoozes respected. No lower bound: a reminder that
    /// is already overdue is the one that most deserves the badge and the
    /// lift, and it is what `SearchResult::due_in`'s "1 h ago" is for.
    pub async fn due_for(&self, artifact_ids: &[String], now: i64, to: i64) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        if artifact_ids.is_empty() {
            return Ok(out);
        }
        let marks = vec!["?"; artifact_ids.len()].join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT artifact_id, MIN(at) AS at FROM moments
             WHERE kind = 'due' AND done_at IS NULL AND at IS NOT NULL AND at < ?
               AND (snoozed_until IS NULL OR snoozed_until <= ?) AND artifact_id IN ({marks})
             GROUP BY artifact_id"
        )))
        .bind(to)
        .bind(now);
        for id in artifact_ids {
            q = q.bind(id);
        }
        for r in q.fetch_all(&self.pool).await? {
            out.insert(r.get("artifact_id"), r.get("at"));
        }
        Ok(out)
    }

    pub async fn event_moments_between(&self, from: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE m.kind = 'event' AND a.status = 'active' AND m.at >= ? AND m.at < ? ORDER BY m.at"
        )))
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    /// Both kinds, any state — the day page's "was due" shows what was done.
    pub async fn moments_between(&self, from: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("{JOINED} WHERE m.at >= ? AND m.at < ? ORDER BY m.at")))
            .bind(from)
            .bind(to)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    pub async fn mark_done(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE moments SET done_at = ? WHERE id = ?").bind(at).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn undo_done(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE moments SET done_at = NULL WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// A snooze that ends re-notifies, so the mark is cleared with it.
    pub async fn snooze(&self, id: &str, until: i64) -> Result<()> {
        sqlx::query("UPDATE moments SET snoozed_until = ?, notified_at = NULL WHERE id = ?")
            .bind(until)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn unsnooze(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE moments SET snoozed_until = NULL WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn mark_notified(&self, ids: &[String], at: i64) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE moments SET notified_at = ? WHERE id = ?").bind(at).bind(id).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn due_unnotified(&self, now: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE {OWED} AND a.status = 'active' AND COALESCE(m.snoozed_until, m.at) <= ? ORDER BY m.at"
        )))
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    /// The Remind unit sleeps until the earliest owed moment. Called by every
    /// write that can move that minimum; a user with no channel never has it
    /// armed, and nothing owed disarms it.
    pub async fn rearm_remind(&self) -> Result<()> {
        use crate::jobs::remind::{notify_targets, REMIND_TARGET};
        use crate::store::jobs::Stage;
        let notify = self.control.notify(&self.subject).await?;
        if notify_targets(&notify).is_empty() {
            return self.disarm(Stage::Remind, REMIND_TARGET).await;
        }
        match self.next_notify_at().await? {
            Some(at) => self.arm_at(Stage::Remind, "collection", REMIND_TARGET, at).await,
            None => self.disarm(Stage::Remind, REMIND_TARGET).await,
        }
    }

    /// The Remind unit's next wake: the earliest owed moment, at any time.
    /// The next second at which the band's contents change, or `None` for
    /// "nothing is coming".
    ///
    /// Three boundaries, because three things move a row: `at - horizon` is
    /// when a moment enters the window `open_due` reads; `at` is when it turns
    /// from coming to overdue; `snoozed_until` is when a row put aside comes
    /// back. The earliest of those still in the future is what the band is
    /// waiting for, and polling before it is asking a question whose answer
    /// cannot have changed.
    pub async fn next_due_change(&self, now: i64, horizon: i64) -> Result<Option<i64>> {
        let r: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT MIN(t) FROM (
               SELECT m.at - ? AS t FROM moments m JOIN artifacts a ON a.id = m.artifact_id
                 WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active' AND m.at IS NOT NULL
               UNION ALL
               SELECT m.at FROM moments m JOIN artifacts a ON a.id = m.artifact_id
                 WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active' AND m.at IS NOT NULL
               UNION ALL
               SELECT m.snoozed_until FROM moments m JOIN artifacts a ON a.id = m.artifact_id
                 WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active'
                   AND m.snoozed_until IS NOT NULL
             ) WHERE t > ?",
        )
        .bind(horizon)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(r.flatten())
    }

    pub async fn next_notify_at(&self) -> Result<Option<i64>> {
        // The same join and the same status filter as `due_unnotified`: what
        // the unit sleeps until must be something the unit will then find. A
        // moment on an artifact that dedupe or a merge has since deprecated is
        // invisible to the read but would still be the minimum here, and the
        // job would wake, find nothing owed, and re-arm itself at that same
        // past instant — forever.
        let r = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT MIN(COALESCE(m.snoozed_until, m.at)) AS at FROM moments m
             JOIN artifacts a ON a.id = m.artifact_id WHERE {OWED} AND a.status = 'active'"
        )))
            .fetch_one(&self.pool)
            .await?;
        Ok(r.get("at"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;
    use crate::store::Store;

    async fn store_with_artifact() -> (Store, String) {
        let s = Store::memory().await.unwrap();
        let c = s.insert_corpus("Remind me friday to send the invoice", "ui", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &c.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "Remind me friday to send the invoice".into(),
                    corpus_span: None,
                    title: Some("Invoice".into()),
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        (s, made[0].id.clone())
    }

    fn due(aid: &str, at: Option<i64>) -> NewMoment {
        NewMoment {
            artifact_id: aid.into(),
            kind: Kind::Due,
            at,
            tz: "Europe/Berlin".into(),
            rule: None,
            source: Source::Cue,
            span: None,
        }
    }

    #[tokio::test]
    async fn open_due_orders_dated_by_time_then_undated() {
        let (s, aid) = store_with_artifact().await;
        let later = s.insert_moment(&due(&aid, Some(2_000))).await.unwrap();
        let none = s.insert_moment(&due(&aid, None)).await.unwrap();
        let soon = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let rows = s.open_due(500, 10_000).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.moment.id.as_str()).collect();
        assert_eq!(ids, vec![soon.as_str(), later.as_str(), none.as_str()]);
        assert_eq!(rows[0].title, "Invoice");
    }

    #[tokio::test]
    async fn done_and_snooze_hide_a_row_and_undo_brings_it_back() {
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.mark_done(&id, 900).await.unwrap();
        assert!(s.open_due(500, 10_000).await.unwrap().is_empty());
        s.undo_done(&id).await.unwrap();
        assert_eq!(s.open_due(500, 10_000).await.unwrap().len(), 1);
        s.snooze(&id, 5_000).await.unwrap();
        assert!(s.open_due(2_000, 10_000).await.unwrap().is_empty(), "snoozed past now");
        assert_eq!(s.open_due(6_000, 10_000).await.unwrap().len(), 1, "snooze elapsed");
    }

    #[tokio::test]
    async fn beyond_the_horizon_is_not_open() {
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(50_000))).await.unwrap();
        assert!(s.open_due(500, 10_000).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_set_row_survives_the_rereading_and_read_rows_do_not() {
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let mut set = due(&aid, Some(2_000));
        set.source = Source::Set;
        s.insert_moment(&set).await.unwrap();
        assert_eq!(s.delete_read_moments(&aid).await.unwrap(), 1);
        let rows = s.open_due(500, 10_000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].moment.source, Source::Set);
    }

    #[tokio::test]
    async fn the_next_wake_is_the_earliest_unnotified_and_snooze_renotifies() {
        let (s, aid) = store_with_artifact().await;
        let a = s.insert_moment(&due(&aid, Some(3_000))).await.unwrap();
        let b = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.insert_moment(&due(&aid, None)).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(1_000));
        s.mark_notified(std::slice::from_ref(&b), 1_001).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(3_000));
        let owed = s.due_unnotified(3_500).await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].moment.id, a);
        s.snooze(&b, 4_000).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(3_000), "b is owed again, after a");
        assert_eq!(s.due_unnotified(4_500).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn due_for_answers_only_the_asked_artifacts_inside_the_window() {
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let hit = s.due_for(&[aid.clone(), "other".into()], 500, 2_000).await.unwrap();
        assert_eq!(hit.get(&aid), Some(&1_000));
        assert!(s.due_for(&["other".into()], 500, 2_000).await.unwrap().is_empty(), "not asked for");
        assert!(s.due_for(std::slice::from_ref(&aid), 500, 900).await.unwrap().is_empty(), "past the window");
    }

    #[tokio::test]
    async fn due_for_keeps_what_is_already_overdue() {
        // The badge and the lift exist for the reminder you have missed as
        // much as for the one ahead; `due_in` renders "1 h ago" for exactly
        // this row.
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let hit = s.due_for(std::slice::from_ref(&aid), 5_000, 9_000).await.unwrap();
        assert_eq!(hit.get(&aid), Some(&1_000), "overdue still lifts");
    }

    #[tokio::test]
    async fn what_the_unit_sleeps_until_is_something_it_will_then_find() {
        // The wake time and the read must agree about the artifact. When they
        // disagree the job wakes, finds nothing owed, re-arms itself at the
        // same past instant, and spins.
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(1_000));
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated).await.unwrap();
        assert!(s.due_unnotified(9_000).await.unwrap().is_empty());
        assert_eq!(s.next_notify_at().await.unwrap(), None, "nothing to find, so nothing to wait for");
    }

    #[tokio::test]
    async fn a_re_read_replaces_what_was_read_and_keeps_what_was_acted_on() {
        // Every embed re-arms the moments stage, so this runs on any reindex
        // or embed-model switch. A reminder already finished or already pushed
        // must not come back from it.
        let (s, aid) = store_with_artifact().await;
        let fresh = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let done = s.insert_moment(&due(&aid, Some(2_000))).await.unwrap();
        let pushed = s.insert_moment(&due(&aid, Some(3_000))).await.unwrap();
        let put_off = s.insert_moment(&due(&aid, Some(4_000))).await.unwrap();
        s.mark_done(&done, 10).await.unwrap();
        s.mark_notified(std::slice::from_ref(&pushed), 10).await.unwrap();
        s.snooze(&put_off, 9_000).await.unwrap();

        assert_eq!(s.delete_read_moments(&aid).await.unwrap(), 1, "only the untouched reading");
        assert!(s.moment(&fresh).await.unwrap().is_none());
        for kept in [&done, &pushed, &put_off] {
            assert!(s.moment(kept).await.unwrap().is_some(), "a row with a history outlives the reading");
        }
        assert!(s.has_moment_at(&aid, Kind::Due, 2_000).await.unwrap(), "and the stage knows not to make it twice");
        assert!(!s.has_moment_at(&aid, Kind::Due, 1_000).await.unwrap());
    }

    #[tokio::test]
    async fn a_counted_recurrence_is_exhausted_by_its_rows() {
        let (s, aid) = store_with_artifact().await;
        let mut m = due(&aid, Some(1_000));
        m.rule = Some("FREQ=DAILY;COUNT=2".into());
        assert!(!s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2").await.unwrap(), "none yet");
        s.insert_moment(&m).await.unwrap();
        assert!(!s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2").await.unwrap());
        s.insert_moment(&m).await.unwrap();
        assert!(s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2").await.unwrap(), "two of two");
        assert!(!s.rule_is_exhausted(&aid, "FREQ=DAILY").await.unwrap(), "open-ended is never exhausted");
    }

    #[tokio::test]
    async fn deleting_the_artifact_takes_its_moments() {
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.delete_artifact(&aid).await.unwrap();
        assert!(s.moment(&id).await.unwrap().is_none());
    }
}
