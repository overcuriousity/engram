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
    /// The next occurrence of a recurrence, armed by the completion of the one
    /// before it. Not a reading of the prose and not a person's doing, and the
    /// distinction is load-bearing: `delete_read_moments` would otherwise take
    /// it for something it read and delete it on the next re-embed, after
    /// which the re-read finds the original instant still done and arms
    /// nothing — the recurrence would end silently at its first completion.
    Armed,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Set => "set",
            Source::Cue => "cue",
            Source::Classified => "classified",
            Source::Extracted => "extracted",
            Source::Armed => "armed",
        }
    }
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "set" => Some(Source::Set),
            "cue" => Some(Source::Cue),
            "classified" => Some(Source::Classified),
            "extracted" => Some(Source::Extracted),
            "armed" => Some(Source::Armed),
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
    /// The instant this row was read at before somebody moved it. `None` on a
    /// row nobody has moved — and also on one that was moved off no instant at
    /// all, which is what dating an undated reminder is. `moved_at` is the mark
    /// that tells those two apart.
    pub moved_from: Option<i64>,
    /// When somebody moved this row, or `None` if nobody has.
    pub moved_at: Option<i64>,
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
        moved_from: r.get("moved_from"),
        moved_at: r.get("moved_at"),
        created_at: r.get("created_at"),
    }
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> DueRow {
    let text: String = r.get("text");
    let opening = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect::<String>();
    let title: Option<String> = r.get("title");
    DueRow { moment: moment_of(r), title: title.filter(|t| !t.is_empty()).unwrap_or_else(|| opening.clone()), opening }
}

/// What a row is due *at* from the reader's point of view: an elapsed snooze
/// is the row's time from the moment it is set, which is why every ladder read
/// coalesces it over `at`.
fn eff_at(r: &DueRow) -> i64 {
    r.moment.snoozed_until.or(r.moment.at).unwrap_or(0)
}

const JOINED: &str = "SELECT m.*, a.title, a.text FROM moments m JOIN artifacts a ON a.id = m.artifact_id";

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
    ///
    /// `a.status = 'active'` because every query that *lists* a reminder says
    /// so. A row on a deprecated or superseded artifact is on no band and has
    /// no button, so nobody can ever complete it; counting it here held the
    /// note open forever on the strength of a reminder that had already
    /// vanished from the screen.
    pub async fn has_open_reminder_for_corpus(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE a.corpus_id = ? AND m.kind = 'due' AND m.done_at IS NULL \
               AND a.status = 'active'",
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
    /// considered, and moving a reminder's date leaves `source` alone — see
    /// `move_moment` — because correcting a date the base read wrong does not
    /// make the note stop having been read as a reminder.
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
    ///
    /// `None` is an instant like any other here, and the comparison is `IS`
    /// rather than `=` so that it matches: an *undated* reminder is the one
    /// case where the stage has nothing to compare but still must not make a
    /// second copy. Read the finished "call the bank" back with `= ?` and NULL
    /// never equals NULL, so every re-read — every reindex, every switched
    /// embed model — put another undated row on the band.
    ///
    /// `moved_from` is consulted for the same reason and answers the other
    /// half of it: a row the operator moved is no longer parked at the instant
    /// the stage read, so without this the next re-read put a fresh row back on
    /// exactly the date they corrected away from — the reminder twice over, and
    /// the wrong one pushing. The second clause is inert when there is no
    /// instant to compare: `moved_from IS NOT NULL` cannot hold while the bound
    /// value is NULL, so an undated reminder still matches on `at` alone and
    /// not on every unmoved row of its kind.
    pub async fn has_moment_at(&self, artifact_id: &str, kind: Kind, at: Option<i64>) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ?
                AND (at IS ? OR (moved_from IS NOT NULL AND moved_from IS ?))",
        )
        .bind(artifact_id)
        .bind(kind.as_str())
        .bind(at)
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

    /// The open due moment on exactly this artifact, dated or not. Unlike
    /// `due_for`, not bounded to a horizon: the pane's own "yes, a reminder
    /// exists here" is a fact about the artifact, not a claim about how soon
    /// it deserves a place on a list.
    pub async fn open_due_for_artifact(&self, artifact_id: &str) -> Result<Option<Moment>> {
        Ok(sqlx::query(
            "SELECT * FROM moments WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL
             ORDER BY at IS NULL, at LIMIT 1",
        )
        .bind(artifact_id)
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
    ///
    /// `moved_at` is a fourth mark of the same kind: a row somebody moved has
    /// been acted on, and it keeps its `source` so that the note still counts
    /// as having been read as a reminder. The mark is `moved_at` and not
    /// `moved_from` because dating a reminder nobody could date is a move off
    /// no instant, and that row has to survive the next re-read just as much.
    ///
    /// `armed` joins `set` as a source this never touches. The successor of a
    /// completed recurrence carries the parent's reading in `source` but is not
    /// itself a reading, and no re-read of the prose would produce it: deleted
    /// here, it was gone for good, because the re-read then found the original
    /// instant still on the artifact — done — and armed nothing in its place.
    pub async fn delete_read_moments(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND source NOT IN ('set', 'armed')
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL
                AND moved_at IS NULL",
        )
            .bind(artifact_id)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Withdraw the reminders this artifact's *reading* put here — the same
    /// rows `delete_read_moments` would drop on a re-read, and no others.
    ///
    /// `set` and `armed` rows are somebody's own, and a row that has been
    /// done, pushed, snoozed or moved has a history of its own; neither is the
    /// stage's to take back. What is left is exactly a verdict nobody has
    /// touched, which is what "this is not a reminder" is answering.
    pub async fn delete_read_due(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND source NOT IN ('set', 'armed')
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL
                AND moved_at IS NULL",
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

    /// Move a reminder to another instant. The row keeps its identity: moving
    /// is not completing, so it leaves no `done` row behind on the day it used
    /// to fall on, and — the reason this is an UPDATE and not the done-plus-
    /// insert it once was — a recurrence still has exactly one row per real
    /// firing, which is what `occurrences_of_rule` counts.
    ///
    /// `source` is left alone. It records how the date got here — a reading or
    /// a person — and a correction to *when* is not a claim about *how*:
    /// overwriting it with `set` made a moved cue reminder stop counting as one
    /// the base had read, and `corpus_was_read_as_reminder` then left the note
    /// un-retired when it was finished. What tells the stage to keep its hands
    /// off is `moved_from`, which is also the misreading itself, kept.
    ///
    /// `COALESCE` so the first move is the one recorded: the instant worth
    /// keeping is the one the base read out of the prose, not whatever the
    /// operator typed on their way to the date they meant.
    ///
    /// The snooze and the notification go with the old date: both were about
    /// an instant that is no longer this row's.
    pub async fn move_moment(&self, id: &str, at: i64, tz: &str) -> Result<()> {
        sqlx::query(
            "UPDATE moments
                SET moved_from = COALESCE(moved_from, at), moved_at = ?, at = ?, tz = ?,
                    snoozed_until = NULL, notified_at = NULL
              WHERE id = ?",
        )
        .bind(crate::store::now())
        .bind(at)
        .bind(tz)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete the occurrence `complete_moment` armed, so that undoing the
    /// completion undoes the whole of it. Untouched rows only: once the next
    /// occurrence has been done, pushed or snoozed in its own right it has a
    /// history of its own, and an undo two steps back does not get to discard
    /// it. Returns whether anything went.
    pub async fn delete_armed_occurrence(&self, artifact_id: &str, rule: &str, at: i64) -> Result<bool> {
        let n = sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND rule = ? AND at = ?
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL",
        )
        .bind(artifact_id)
        .bind(rule)
        .bind(at)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// A snooze that ends re-notifies, so the mark is cleared with it.
    ///
    /// Clearing it is only safe because a snoozed row climbs a one-rung
    /// ladder — see [`crate::jobs::remind::owed_lead`]. `eff_at` becomes
    /// `snoozed_until`, so with the full ladder every lead above the snooze's
    /// own length sat *behind* the new time and the cleared mark owed one of
    /// them at once: the operator put a reminder aside for an hour and the
    /// phone said it again on the next queue tick.
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

    /// What owes a push at `now`: every row standing on a rung of the ladder
    /// it has not been pushed on yet.
    ///
    /// The read is deliberately loose and the ladder is applied in Rust, over
    /// [`crate::jobs::remind::owed_lead`]. The rungs are one list in one
    /// place; spelling them a second time as a SQL `UNION` would be two
    /// definitions of when a reminder is due to be said out loud, and the
    /// number of open reminders in a base is small enough that the difference
    /// is not measurable.
    pub async fn due_owed(&self, now: i64) -> Result<Vec<DueRow>> {
        let rows: Vec<DueRow> = self.uncovered().await?;
        Ok(rows
            .into_iter()
            .filter(|r| {
                crate::jobs::remind::owed_lead(
                    eff_at(r),
                    r.moment.snoozed_until.is_some(),
                    r.moment.notified_at,
                    now,
                )
                .is_some()
            })
            .collect())
    }

    /// Every dated, undone, live due row whose last push does not already
    /// cover the whole ladder — the candidates both ladder reads work from.
    async fn uncovered(&self) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE m.kind = 'due' AND m.done_at IS NULL AND m.at IS NOT NULL
               AND a.status = 'active'
               AND (m.notified_at IS NULL OR m.notified_at < COALESCE(m.snoozed_until, m.at))
             ORDER BY COALESCE(m.snoozed_until, m.at)"
        )))
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

    /// The Remind unit's next wake: the earliest rung any row still owes, at
    /// any time. A rung already behind us is returned as it is, so a unit
    /// armed at it fires at once.
    pub async fn next_notify_at(&self) -> Result<Option<i64>> {
        // The same read as `due_owed`, for the same reason it has always been
        // the same read: what the unit sleeps until must be something the unit
        // will then find. A moment on an artifact that dedupe or a merge has
        // since deprecated would otherwise be the minimum here, and the job
        // would wake, find nothing owed, and re-arm itself at that same past
        // instant — forever.
        Ok(self
            .uncovered()
            .await?
            .iter()
            .filter_map(|r| {
                crate::jobs::remind::next_lead_at(
                    eff_at(r),
                    r.moment.snoozed_until.is_some(),
                    r.moment.notified_at,
                )
            })
            .min())
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
    async fn the_next_wake_is_the_earliest_rung_owed_and_snooze_renotifies() {
        use crate::jobs::remind::LEADS;
        let (s, aid) = store_with_artifact().await;
        let a = s.insert_moment(&due(&aid, Some(10_000_000))).await.unwrap();
        let b = s.insert_moment(&due(&aid, Some(9_000_000))).await.unwrap();
        s.insert_moment(&due(&aid, None)).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(9_000_000 - LEADS[0]), "the nearer moment's first rung");
        s.mark_notified(std::slice::from_ref(&b), 9_000_000).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(10_000_000 - LEADS[0]), "b is said and done");
        let owed = s.due_owed(10_000_000 - LEADS[0]).await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].moment.id, a);
        s.snooze(&b, 11_000_000).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(10_000_000 - LEADS[0]), "b is owed again, after a");
        assert_eq!(s.due_owed(11_000_000).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_row_is_owed_a_push_at_each_rung_of_the_ladder_and_not_between_them() {
        use crate::jobs::remind::LEADS;
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        assert!(s.due_owed(at - LEADS[0] - 1).await.unwrap().is_empty(), "not yet in the band");

        let owed = s.due_owed(at - LEADS[0]).await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].moment.id, id);
        s.mark_notified(std::slice::from_ref(&id), at - LEADS[0]).await.unwrap();

        assert!(s.due_owed(at - LEADS[1] - 1).await.unwrap().is_empty(), "the rung is taken");
        assert_eq!(s.due_owed(at - LEADS[1]).await.unwrap().len(), 1, "the next one comes round");
    }

    #[tokio::test]
    async fn the_unit_sleeps_until_the_next_rung_not_until_the_moment() {
        use crate::jobs::remind::LEADS;
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(at - LEADS[0]));
        s.mark_notified(std::slice::from_ref(&id), at - LEADS[0]).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(at - LEADS[1]));
        s.mark_notified(std::slice::from_ref(&id), at).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), None, "the last rung was the moment itself");
    }

    #[tokio::test]
    async fn a_snooze_is_said_once_when_it_ends_and_not_before() {
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        s.mark_notified(std::slice::from_ref(&id), at).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), None);

        // An hour, which is shorter than every lead above the last one. On the
        // full ladder the cleared `notified_at` owed one of those rungs at
        // once, and the operator was pushed the reminder they had just put
        // aside, on the next queue tick.
        let until = at + 3_600;
        s.snooze(&id, until).await.unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(until), "the snooze's own end, and nothing sooner");
        assert!(s.due_owed(at + 1).await.unwrap().is_empty(), "not the second it was put aside");
        assert!(s.due_owed(until - 1).await.unwrap().is_empty(), "nor at any lead inside it");

        let owed = s.due_owed(until).await.unwrap();
        assert_eq!(owed.len(), 1, "and then it comes back, once");
        assert_eq!(owed[0].moment.id, id);
        s.mark_notified(std::slice::from_ref(&id), until).await.unwrap();
        assert!(s.due_owed(until + 10_000).await.unwrap().is_empty(), "one rung, said");
        assert_eq!(s.next_notify_at().await.unwrap(), None);
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

    /// Unlike `due_for`, no horizon: an artifact's own pane asks whether it
    /// carries a reminder at all, not whether one is close enough to belong
    /// on a list.
    #[tokio::test]
    async fn open_due_for_artifact_ignores_the_horizon_but_not_done_or_undated() {
        let (s, aid) = store_with_artifact().await;
        assert!(s.open_due_for_artifact(&aid).await.unwrap().is_none(), "nothing set yet");

        let far = s.insert_moment(&due(&aid, Some(50_000_000))).await.unwrap();
        let hit = s.open_due_for_artifact(&aid).await.unwrap().unwrap();
        assert_eq!(hit.id, far, "far outside any horizon, still the pane's own reminder");

        s.mark_done(&far, 900).await.unwrap();
        assert!(s.open_due_for_artifact(&aid).await.unwrap().is_none(), "done is not open");

        s.insert_moment(&due(&aid, None)).await.unwrap();
        let undated = s.open_due_for_artifact(&aid).await.unwrap().unwrap();
        assert!(undated.at.is_none(), "an undated reminder is still open, just not lifted by ago_or_ahead");
    }

    #[tokio::test]
    async fn what_the_unit_sleeps_until_is_something_it_will_then_find() {
        // The wake time and the read must agree about the artifact. When they
        // disagree the job wakes, finds nothing owed, re-arms itself at the
        // same past instant, and spins.
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        assert!(s.next_notify_at().await.unwrap().is_some());
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated).await.unwrap();
        assert!(s.due_owed(9_000).await.unwrap().is_empty());
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
        assert!(s.has_moment_at(&aid, Kind::Due, Some(2_000)).await.unwrap(), "and the stage knows not to make it twice");
        assert!(!s.has_moment_at(&aid, Kind::Due, Some(1_000)).await.unwrap());
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
    async fn a_moved_row_keeps_its_reading_and_outlives_the_next_re_read() {
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.move_moment(&id, 5_000, "Europe/Berlin").await.unwrap();

        let m = s.moment(&id).await.unwrap().unwrap();
        assert_eq!(m.at, Some(5_000));
        assert_eq!(m.moved_from, Some(1_000), "the misreading is kept");
        assert!(m.moved_at.is_some());
        assert_eq!(m.source, Source::Cue, "moving says nothing about how the date got here");

        assert_eq!(s.delete_read_moments(&aid).await.unwrap(), 0, "a moved row has a history");
        assert!(
            s.has_moment_at(&aid, Kind::Due, Some(1_000)).await.unwrap(),
            "and the stage will not put a fresh one back on the date that was corrected away from"
        );

        // A second move keeps the first instant: what is worth keeping is what
        // the base read, not the operator's own way to the date they meant.
        s.move_moment(&id, 9_000, "Europe/Berlin").await.unwrap();
        assert_eq!(s.moment(&id).await.unwrap().unwrap().moved_from, Some(1_000));
    }

    #[tokio::test]
    async fn dating_an_undated_reminder_is_a_move_with_no_instant_to_keep() {
        // The one move with nothing to record in `moved_from`, and the row has
        // to survive the next re-read all the same — which is why the mark is
        // `moved_at` and not `moved_from`.
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, None)).await.unwrap();
        s.move_moment(&id, 5_000, "Europe/Berlin").await.unwrap();
        let m = s.moment(&id).await.unwrap().unwrap();
        assert_eq!(m.moved_from, None);
        assert!(m.moved_at.is_some());
        assert_eq!(s.delete_read_moments(&aid).await.unwrap(), 0, "and it is not read away");
    }

    #[tokio::test]
    async fn an_armed_occurrence_is_not_what_the_stage_read() {
        let (s, aid) = store_with_artifact().await;
        let mut m = due(&aid, Some(1_000));
        m.source = Source::Armed;
        let armed = s.insert_moment(&m).await.unwrap();
        let read = s.insert_moment(&due(&aid, Some(2_000))).await.unwrap();
        assert_eq!(s.delete_read_moments(&aid).await.unwrap(), 1, "only the reading");
        assert!(s.moment(&armed).await.unwrap().is_some());
        assert!(s.moment(&read).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_reminder_nobody_can_reach_does_not_hold_its_note_open() {
        // Every query that lists a reminder says `status = 'active'`. On a
        // deprecated artifact the row is on no band and has no button, so
        // counting it held the note open on a reminder nobody could finish.
        let (s, aid) = store_with_artifact().await;
        let cid = s.get_artifact(&aid).await.unwrap().corpus_id.unwrap();
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        assert!(s.has_open_reminder_for_corpus(&cid).await.unwrap());
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated).await.unwrap();
        assert!(!s.has_open_reminder_for_corpus(&cid).await.unwrap());
    }

    #[tokio::test]
    async fn deleting_the_artifact_takes_its_moments() {
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.delete_artifact(&aid).await.unwrap();
        assert!(s.moment(&id).await.unwrap().is_none());
    }
}
