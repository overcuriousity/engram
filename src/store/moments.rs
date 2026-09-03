//! A time attached to an artifact. See the schema comment and the spec's §3.

use crate::error::Result;
use crate::store::{Store, new_id};
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
    /// distinction is load-bearing: `delete_read_due` would otherwise take
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
    /// The recurrence this row is one occurrence of — the id of the series'
    /// first moment, which for that first moment is its own id. `None` on a
    /// one-shot, and on every row written before the column existed.
    pub series_id: Option<String>,
    /// The note this row was read out of, as it stood when the row was
    /// written. `None` on a row that predates the column, where the artifact
    /// it sits on is the only answer there is.
    pub origin_corpus_id: Option<String>,
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
    /// The series this row continues, for the successor a completed
    /// recurrence arms. `None` everywhere else: a row that carries a rule and
    /// continues nothing is the first occurrence of its own series, and
    /// `insert_moment` stamps it with its own id.
    pub series_id: Option<String>,
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
        series_id: r.get("series_id"),
        origin_corpus_id: r.get("origin_corpus_id"),
        created_at: r.get("created_at"),
    }
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> DueRow {
    let text: String = r.get("text");
    let opening = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect::<String>();
    let title: Option<String> = r.get("title");
    DueRow {
        moment: moment_of(r),
        title: title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| opening.clone()),
        opening,
    }
}

/// What a row is due *at* from the reader's point of view: an elapsed snooze
/// is the row's time from the moment it is set, which is why every ladder read
/// coalesces it over `at`.
fn eff_at(r: &DueRow) -> i64 {
    r.moment.snoozed_until.or(r.moment.at).unwrap_or(0)
}

const JOINED: &str =
    "SELECT m.*, a.title, a.text FROM moments m JOIN artifacts a ON a.id = m.artifact_id";

impl Store {
    /// The note this moment was read out of.
    ///
    /// `origin_corpus_id` first, and the artifact's corpus only where the row
    /// predates that column. A moment hangs off an artifact, but not always
    /// the one it was written on: `carry_moments` moves an open row onto the
    /// artifact that superseded its own, and where a promotion or a merge
    /// crossed notes, the join alone then answered a note nobody had set a
    /// reminder on — which `complete_moment` duly retired, sinking a note out
    /// of `recent_captures` while the one that was actually read stayed put.
    pub async fn corpus_of_moment(&self, moment_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT COALESCE(m.origin_corpus_id, a.corpus_id) \
             FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
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
             WHERE COALESCE(m.origin_corpus_id, a.corpus_id) = ? AND m.kind = 'due' \
               AND m.done_at IS NULL AND a.status = 'active'",
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
             WHERE COALESCE(m.origin_corpus_id, a.corpus_id) = ? \
               AND m.source IN ('cue', 'classified')",
        )
        .bind(corpus_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Two columns this fills in for itself, because both answer questions
    /// about where the row came from and neither can be re-derived later.
    ///
    /// `series_id` is the row's own id where the row carries a rule and
    /// continues no series — the first occurrence of a recurrence names the
    /// series — and the parent's where `NewMoment` names one. A one-shot has
    /// none. `occurrences_in_series` is what reads it.
    ///
    /// `origin_corpus_id` is the note the artifact belonged to at this
    /// instant. `carry_moments` will move the row onto another artifact, and
    /// after that the join through `artifact_id` answers a different note than
    /// the one whose prose was read as the reminder.
    pub async fn insert_moment(&self, m: &NewMoment) -> Result<String> {
        let id = new_id();
        let series: Option<String> = match (&m.series_id, &m.rule) {
            (Some(s), _) => Some(s.clone()),
            (None, Some(_)) => Some(id.clone()),
            (None, None) => None,
        };
        let corpus: Option<String> =
            sqlx::query_scalar("SELECT corpus_id FROM artifacts WHERE id = ?")
                .bind(&m.artifact_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        sqlx::query(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, rule, source, span,
                                  series_id, origin_corpus_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&m.artifact_id)
        .bind(m.kind.as_str())
        .bind(m.at)
        .bind(&m.tz)
        .bind(&m.rule)
        .bind(m.source.as_str())
        .bind(&m.span)
        .bind(&series)
        .bind(&corpus)
        .bind(crate::store::now())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Does this artifact already carry a moment of this kind at this
    /// instant? The moments stage asks before it re-inserts what it just read,
    /// so a row `delete_read_due` kept — one already done, pushed or
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
    ///
    /// The third clause is the undated probe's own half of the moved story.
    /// Dating a reminder nobody could date is a move off no instant —
    /// `move_moment` leaves `moved_from` NULL and stamps `moved_at` — so
    /// neither of the first two clauses can hold for it, and the next re-read
    /// of the same prose put a second undated row beside the dated one. A row
    /// moved off nothing *is* the undated reading, so the undated probe reads
    /// `moved_at` where `moved_from` has nothing to say.
    pub async fn has_moment_at(
        &self,
        artifact_id: &str,
        kind: Kind,
        at: Option<i64>,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ?
                AND (at IS ?
                     OR (moved_from IS NOT NULL AND moved_from IS ?)
                     OR (? IS NULL AND moved_at IS NOT NULL AND moved_from IS NULL))",
        )
        .bind(artifact_id)
        .bind(kind.as_str())
        .bind(at)
        .bind(at)
        .bind(at)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Move an artifact's open moments to the artifact that superseded it.
    ///
    /// `supersede` already says that every other question about the loser now
    /// belongs to the winner — the open pairs move, the links move, the
    /// engagement moves. Time did not, and it is the one that pushes: a
    /// reminder read onto a verbatim passage, which is what the judgement
    /// anchors to when the model wrote no artifact for a note that is nothing
    /// but a reminder, went dark the moment a later promotion superseded that
    /// passage. `uncovered` filters on `a.status = 'active'`, so the row simply
    /// stopped being on the band — no push at any rung, nothing on the day
    /// page, and nothing anywhere saying it had gone.
    ///
    /// Open rows only. A done row is the history of a thing that happened to
    /// the artifact it happened on, and moving it would rewrite that.
    ///
    /// And never onto an instant the winner already carries, which is the same
    /// uniqueness every insert on an artifact asks about: a promotion whose
    /// artifacts were themselves read as the reminder has the row already, and
    /// the passage's copy stays where it is rather than becoming a second one.
    pub async fn carry_moments(&self, loser: &str, winner: &str) -> Result<u64> {
        if loser == winner {
            return Ok(0);
        }
        let res = sqlx::query(
            "UPDATE moments SET artifact_id = ?
              WHERE artifact_id = ? AND done_at IS NULL
                AND NOT EXISTS (SELECT 1 FROM moments w
                                 WHERE w.artifact_id = ?
                                   AND w.kind = moments.kind
                                   AND w.at IS moments.at)",
        )
        .bind(winner)
        .bind(loser)
        .bind(winner)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// The wall-clock this recurrence is meant to happen at, as an instant to
    /// read a time off.
    ///
    /// A row moved by hand wins, most recent first: moving a recurring
    /// reminder from 09:00 to 10:00 is a person restating what time it
    /// happens, and every later occurrence should follow. Failing that, the
    /// first row of the series — the time the recurrence was read at, before
    /// any daylight-saving gap rolled an occurrence forward off it.
    ///
    /// `None` where the series has no dated row, which leaves `next_after`
    /// reading the previous occurrence exactly as it always did.
    pub async fn series_anchor(&self, series_id: &str) -> Result<Option<i64>> {
        let moved: Option<i64> = sqlx::query_scalar(
            "SELECT at FROM moments
              WHERE series_id = ? AND at IS NOT NULL AND moved_at IS NOT NULL
              ORDER BY moved_at DESC LIMIT 1",
        )
        .bind(series_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        if moved.is_some() {
            return Ok(moved);
        }
        Ok(sqlx::query_scalar(
            "SELECT at FROM moments
              WHERE series_id = ? AND at IS NOT NULL
              ORDER BY created_at, id LIMIT 1",
        )
        .bind(series_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Is a moment of this kind actually parked at this instant?
    ///
    /// `has_moment_at` above answers a wider question on purpose — it also says
    /// yes for an instant that is merely some row's `moved_from`, because a
    /// re-read landing back on the date the operator corrected away from must
    /// not put a second row there. That is the right question for a re-read and
    /// the wrong one for arming a recurrence: `complete_moment` asks "does this
    /// artifact already carry the successor", and a daily reminder moved by
    /// hand from Wednesday to Tuesday answers its own next Wednesday with the
    /// instant it was moved off. Nothing was armed, and `has_moved_moment` then
    /// refused every re-read — so the recurrence died on its first completion,
    /// silently and for good.
    ///
    /// So this one asks only what it needs: is there a row *here*. A row moved
    /// *onto* this instant carries it in `at` and is still found; a row moved
    /// *off* it is somewhere else and no longer speaks for it.
    pub async fn moment_parked_at(
        &self,
        artifact_id: &str,
        kind: Kind,
        at: Option<i64>,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ? AND at IS ?",
        )
        .bind(artifact_id)
        .bind(kind.as_str())
        .bind(at)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Is there an open due row here that has already been acted on — pushed,
    /// or put aside?
    ///
    /// `delete_read_due` deliberately keeps such a row: it has a history, and
    /// a row with a history outlives the reading that made it. What the stage
    /// then did with that fact was wrong, though. It deleted, found nothing to
    /// delete, and inserted anyway — so a note re-read as a reminder after its
    /// 48 h rung had fired came out carrying two open rows for one reading of
    /// one piece of prose, both climbing the ladder and both pushing.
    ///
    /// So the same stance `has_moved_moment` takes for a row a person moved: a
    /// row the base has already spoken about is not this re-read's to replace.
    pub async fn has_acted_on_due(&self, artifact_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL
                AND (notified_at IS NOT NULL OR snoozed_until IS NOT NULL)",
        )
        .bind(artifact_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Hand the recurrence a re-read has just recognised to the open row
    /// already parked at that instant.
    ///
    /// The stage's "this instant is already here" guard compares instants and
    /// nothing else, which is right for the duplicate it exists to prevent and
    /// wrong for the one thing a second reading of the same prose can still
    /// add: the same Friday, now understood as *every* Friday. The guard
    /// returned first, the rule was dropped, and the reminder stayed a
    /// one-shot however often the model read the recurrence correctly.
    ///
    /// A row gaining a rule becomes the head of its own series, which is what
    /// `COUNT` is later counted over — see `occurrences_in_series`.
    pub async fn set_rule_of_open_due(
        &self,
        artifact_id: &str,
        at: Option<i64>,
        rule: &str,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE moments SET rule = ?, series_id = COALESCE(series_id, id)
              WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL AND at IS ?
                AND (rule IS NULL OR rule <> ?)",
        )
        .bind(rule)
        .bind(artifact_id)
        .bind(at)
        .bind(rule)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Has the operator moved a moment of this kind on this artifact?
    ///
    /// The re-read's other guard, and the one `has_moment_at` cannot be: that
    /// answers "is this exact instant already here", which suppresses a
    /// re-read that lands back on the corrected-away-from date and nothing
    /// else. A model reading the same prose a third time and resolving it a
    /// third way — Fri 14:00 read, moved to 16:00 by hand, re-read as 15:00 —
    /// matched neither clause, and a second open row appeared beside the
    /// correction with both of them pushing.
    ///
    /// A person's date outranks a re-reading of the prose it was corrected
    /// from. Only the read is refused: completion still arms the recurrence's
    /// next occurrence, and a door that sets a moment still sets one.
    pub async fn has_moved_moment(&self, artifact_id: &str, kind: Kind) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ? AND moved_at IS NOT NULL",
        )
        .bind(artifact_id)
        .bind(kind.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// How many occurrences of one recurrence have existed on this artifact,
    /// done rows included — the history of a recurring reminder is its rows,
    /// so counting them is counting the occurrences.
    pub async fn occurrences_of_rule(&self, artifact_id: &str, rule: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM moments WHERE artifact_id = ? AND kind = 'due' AND rule = ?",
        )
        .bind(artifact_id)
        .bind(rule)
        .fetch_one(&self.pool)
        .await?)
    }

    /// How many occurrences one recurrence has had, counted by its series and
    /// so immune to the artifact its rows sit on.
    ///
    /// `occurrences_of_rule` above counts the rows on one artifact, which is
    /// the same number right up until a supersession: `carry_moments` moves
    /// the *open* row to the winner and leaves the done ones on the loser, so
    /// a `COUNT=3` that had already fired twice arrived at the winner counting
    /// one, re-armed past its count, and — since every later occurrence landed
    /// on the winner too — never stopped. A bounded recurrence became an
    /// unbounded one, silently.
    pub async fn occurrences_in_series(&self, series_id: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM moments WHERE kind = 'due' AND series_id = ?")
                .bind(series_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Has a `COUNT=n` rule had its n occurrences? False for a rule with no
    /// COUNT, which is open-ended, and for one that does not parse — an
    /// unreadable rule is `next_after`'s to refuse, not this read's.
    ///
    /// Counted over the series where the row has one, and over the artifact's
    /// rows where it does not — which is every row written before `series_id`
    /// existed, and is what those rows have always been counted by.
    pub async fn rule_is_exhausted(
        &self,
        artifact_id: &str,
        rule: &str,
        series_id: Option<&str>,
    ) -> Result<bool> {
        let Some(count) = crate::core::moments::rule_count(rule) else {
            return Ok(false);
        };
        let n = match series_id {
            Some(s) => self.occurrences_in_series(s).await?,
            None => self.occurrences_of_rule(artifact_id, rule).await?,
        };
        Ok(n >= count as i64)
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

    /// The dates a note merely *states*, as the previous reading of it left
    /// them, so a re-read replaces rather than duplicates.
    ///
    /// The guards are the ones `delete_read_due` explains at length, and the
    /// reason for two functions rather than one is timing, not predicate: the
    /// stage decides the two kinds at different moments. What a note states is
    /// settled as soon as the reply parses, so the previous reading of it can
    /// go straight away. Whether the note *is* a reminder is settled much
    /// further down, past several `return`s that file nothing at all — and a
    /// due row deleted before those was a standing reminder destroyed by a
    /// re-read with nothing to put in its place. See `jobs::judgement::apply`.
    pub async fn delete_read_events(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'event' AND source NOT IN ('set', 'armed')
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL
                AND moved_at IS NULL",
        )
        .bind(artifact_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Withdraw the reminders this artifact's *reading* put here, and no
    /// others — what a re-read that reads it differently drops, and what "this
    /// is not a reminder" is answering.
    ///
    /// A row somebody set is not the stage's to delete, and neither is one
    /// that has since been acted on. Every embed re-arms this stage, so
    /// without the second half a reindex or a switched embed model would
    /// delete a reminder finished months ago and read it back fresh: it would
    /// return to the band and push again. `done_at`, `notified_at` and
    /// `snoozed_until` are the three marks that say a row has a history, and a
    /// row with a history outlives the reading that made it.
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

    /// "This is not a reminder", said by a person, on purpose.
    ///
    /// The guards on `delete_read_due` above are all defences against a
    /// *re-read* — the stage arriving at the same instant a second time and
    /// treating a row with a history as its own to delete. None of them
    /// defends against the operator, and applied to them they made the button
    /// a no-op on almost every row it was offered on: `LEADS[0]` is 48 h and
    /// `time.horizon_hours` defaults to 48, so a dated row enters the band and
    /// takes its first push at the same instant, and from then on
    /// `notified_at IS NULL` matched nothing. The band said "Not a reminder"
    /// and the row went on pushing at 12 h, 3 h, 30 min and zero.
    ///
    /// So: every row on this artifact the stage is responsible for, whatever
    /// has happened to it since. `armed` goes too — a successor occurrence
    /// exists only because of the reading now being disclaimed, and leaving it
    /// standing would re-push tomorrow the thing that was never a reminder.
    /// `set` stays, and is the one source the button is never offered for: a
    /// reminder somebody typed is not a misreading. `done_at` stays too —
    /// completed rows are history, they are not on the band, and the operator
    /// is not reaching for them.
    ///
    /// Returns whether anything went, so the band can keep quiet rather than
    /// announce an undo for a row still sitting in front of the reader.
    pub async fn delete_refused_due(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND source <> 'set' AND done_at IS NULL",
        )
        .bind(artifact_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Open reminders: undone, on an active artifact, and either undated or
    /// due before `to` and not snoozed past `now`. Dated first, by time —
    /// the *effective* time: a row whose snooze has elapsed re-enters the
    /// band at the instant the operator named, not sorted as most overdue by
    /// the date they put aside.
    pub async fn open_due(&self, now: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active'
               AND (m.at IS NULL OR (m.at < ? AND (m.snoozed_until IS NULL OR m.snoozed_until <= ?)))
             ORDER BY m.at IS NULL, COALESCE(m.snoozed_until, m.at), m.created_at"
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
    pub async fn due_for(
        &self,
        artifact_ids: &[String],
        now: i64,
        to: i64,
    ) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        if artifact_ids.is_empty() {
            return Ok(out);
        }
        let marks = vec!["?"; artifact_ids.len()].join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            // The *effective* instant, as every other ladder read takes it —
            // see `eff_at` and `open_due`'s ordering. A row snoozed from Monday
            // to Friday is correctly re-admitted on Friday, and reading `at`
            // here made the badge say "4 days ago" for a reminder due in an
            // hour and gave the search lift the instant the operator put aside.
            "SELECT artifact_id, MIN(COALESCE(snoozed_until, at)) AS at FROM moments
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
    ///
    /// "Any state" is the *moment's*: done rows belong on the day they were
    /// due. The artifact's is filtered like every other read on this table.
    /// Alone among them this one was not, and a moment that rode a supersession
    /// onto the replacement artifact was then listed twice on the day page —
    /// once against the row a reader can open and once against the retired one
    /// behind it.
    ///
    /// Done rows are the exception, and they have to be: `carry_moments` moves
    /// only *open* rows, so a reminder that was completed and whose artifact
    /// was later superseded by an ordinary promotion had nothing left to
    /// stand on. The status filter alone took its "was due / done" line off
    /// the day page for good — which is the one thing a day page exists to
    /// keep. A done row beside a live one reads as two lines in two different
    /// states, not as the duplicate the filter was added for.
    pub async fn moments_between(&self, from: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{JOINED} WHERE (a.status = 'active' OR m.done_at IS NOT NULL) \
               AND m.at >= ? AND m.at < ? ORDER BY m.at"
        )))
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_of).collect())
    }

    /// Claim a moment as done, once. Returns whether *this* call is the one
    /// that completed it.
    ///
    /// The `done_at IS NULL` clause is the whole of the guard `complete_moment`
    /// needs: a recurrence arms its successor after marking, and two presses of
    /// "done" arriving together — a double-click, a retried htmx post — both
    /// read an open row, both marked it, and both armed. Two rows for tomorrow,
    /// and a `COUNT=5` that ends after three firings because the count is the
    /// rows. One statement decides it instead.
    pub async fn mark_done(&self, id: &str, at: i64) -> Result<bool> {
        Ok(
            sqlx::query("UPDATE moments SET done_at = ? WHERE id = ? AND done_at IS NULL")
                .bind(at)
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    pub async fn undo_done(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE moments SET done_at = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
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
    ///
    /// `source = 'armed'` is what makes the name true, and it was missing.
    /// `complete_moment` arms a successor only where the artifact does not
    /// already carry that instant — a re-read that landed on it, or an
    /// occurrence somebody set by hand — so on an artifact with two same-rule
    /// due rows the completion armed nothing and the undo deleted the *other*
    /// row outright. Undo is not allowed to take away a row this completion
    /// never created.
    pub async fn delete_armed_occurrence(
        &self,
        artifact_id: &str,
        rule: &str,
        at: i64,
    ) -> Result<bool> {
        let n = sqlx::query(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND rule = ? AND at = ?
                AND source = 'armed'
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

    /// A snooze that ends re-notifies on its own: `eff_at` becomes
    /// `snoozed_until`, so whatever `notified_at` holds is behind the new
    /// time and the one-rung ladder owes the snooze's end regardless — see
    /// [`crate::jobs::remind::owed_lead`]. The mark is deliberately kept.
    /// It used to be cleared here, which the snooze itself never needed, and
    /// an *unsnooze* then found a bare ladder: the reminder the phone had
    /// already said was owed — and said — again the moment the snooze was
    /// taken back.
    /// Whether a row was actually put aside, so the band can say "Snoozed"
    /// only where something was. An id that names nothing — a stale button on
    /// a page open since before a re-read replaced the row — reported the
    /// snooze and offered the undo for it either way.
    /// `at IS NOT NULL`, because there is nothing to put aside. An undated
    /// reminder is the band's question "when?", and `open_due` lists it
    /// unconditionally — the `m.at IS NULL` branch short-circuits the snooze
    /// predicate, and it has to, or a row asking for a date could be hidden
    /// away from the only field that supplies one. So a snooze accepted here
    /// was a snooze that hid nothing: the API answered 204, the band drew
    /// "Snoozed — undo", and the row went on standing directly underneath it
    /// for ever. The band never offered the control on such a row; only the
    /// API door reached it.
    pub async fn snooze(&self, id: &str, until: i64) -> Result<bool> {
        Ok(
            sqlx::query(
                "UPDATE moments SET snoozed_until = ?                  WHERE id = ? AND done_at IS NULL AND at IS NOT NULL",
            )
                .bind(until)
                .bind(id)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    pub async fn unsnooze(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE moments SET snoozed_until = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One statement for the whole batch: the caller has already sent one
    /// push covering every id, so a failure part-way through a row-at-a-time
    /// loop left half the batch marked — and the retry that the error buys
    /// then re-delivered the other half as a second push.
    pub async fn mark_notified(&self, ids: &[String], at: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE moments SET notified_at = ? WHERE id IN ({marks})"
        )))
        .bind(at);
        for id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
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
                    r.moment.created_at,
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
        use crate::jobs::remind::{REMIND_TARGET, notify_targets};
        use crate::store::jobs::Stage;
        let notify = self.control.notify(&self.subject).await?;
        if notify_targets(&notify).is_empty() {
            return self.disarm(Stage::Remind, REMIND_TARGET).await;
        }
        match self.next_notify_at().await? {
            Some(at) => {
                self.arm_at(Stage::Remind, "collection", REMIND_TARGET, at)
                    .await
            }
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
                    r.moment.created_at,
                )
            })
            .min())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::artifacts::NewArtifact;

    async fn store_with_artifact() -> (Store, String) {
        let s = Store::memory().await.unwrap();
        let c = s
            .insert_corpus("Remind me friday to send the invoice", "ui", None)
            .await
            .unwrap();
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

    /// A second note with one artifact, so a carry can cross corpora the way a
    /// merge does.
    async fn other_artifact(s: &Store, text: &str) -> String {
        let c = s.insert_corpus(text, "ui", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &c.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: text.into(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        made[0].id.clone()
    }

    /// A done reminder belongs to the day it was due, whatever later became of
    /// the artifact it hung off. `carry_moments` moves only *open* rows, so an
    /// ordinary promotion superseding the artifact underneath a completed
    /// reminder left that row with nothing active to stand on — and the status
    /// filter, added to stop one reminder being listed twice, took its "was
    /// due / done" line off the day page for good.
    #[tokio::test]
    async fn a_completed_reminder_keeps_its_day_after_its_artifact_is_superseded() {
        let (s, aid) = store_with_artifact().await;
        let id = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(1_000),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        assert!(s.mark_done(&id, 1_100).await.unwrap());

        let winner = other_artifact(&s, "the passage that replaced it").await;
        s.set_superseded_by(&aid, Some(&winner)).await.unwrap();
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();

        let rows = s.moments_between(0, 2_000).await.unwrap();
        assert!(
            rows.iter().any(|r| r.moment.id == id),
            "the day it was due still says it happened"
        );
        // And the filter still does its own job: an *open* row on a retired
        // artifact is not on any band and has no business on the day page.
        let open = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(1_500),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let rows = s.moments_between(0, 2_000).await.unwrap();
        assert!(
            !rows.iter().any(|r| r.moment.id == open),
            "and an open row behind a supersession is still filtered out"
        );
    }

    /// There is nothing to put an undated reminder aside *until*: it is the
    /// band's question "when?", and `open_due` lists it whatever any snooze
    /// says. A snooze accepted on one hid nothing at all — the API answered
    /// 204, the band drew "Snoozed — undo", and the row went on standing
    /// directly underneath it.
    #[tokio::test]
    async fn a_reminder_with_no_date_cannot_be_put_aside() {
        let (s, aid) = store_with_artifact().await;
        let undated = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: None,
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        assert!(
            !s.snooze(&undated, 10_000).await.unwrap(),
            "nothing moved, and the caller is told so"
        );
        assert!(
            s.moment(&undated)
                .await
                .unwrap()
                .unwrap()
                .snoozed_until
                .is_none(),
            "and no snooze was written"
        );
        assert!(
            s.open_due(1_000, 5_000)
                .await
                .unwrap()
                .iter()
                .any(|r| r.moment.id == undated),
            "the row is where it always was"
        );

        // A dated row is untouched by the guard.
        let dated = s
            .insert_moment(&NewMoment {
                artifact_id: aid,
                kind: Kind::Due,
                at: Some(2_000),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        assert!(s.snooze(&dated, 10_000).await.unwrap());
    }

    /// `carry_moments` moves the *open* row of a recurrence to the winner and
    /// leaves the done ones behind, so counting the rows on one artifact
    /// restarted the count at every supersession: a `COUNT=3` that had fired
    /// twice arrived counting one and re-armed for ever.
    #[tokio::test]
    async fn a_counted_recurrence_is_counted_by_its_series_not_its_artifact() {
        let (s, aid) = store_with_artifact().await;
        let rule = "FREQ=DAILY;COUNT=3";
        let first = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(1_000),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let series = s.moment(&first).await.unwrap().unwrap().series_id.unwrap();
        assert_eq!(series, first, "the first occurrence names the series");
        s.mark_done(&first, 1_100).await.unwrap();
        let second = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(87_400),
                tz: "Europe/Berlin".into(),
                rule: Some(rule.into()),
                source: Source::Armed,
                span: None,
                series_id: Some(series.clone()),
            })
            .await
            .unwrap();

        let winner = other_artifact(&s, "the promoted rewrite").await;
        assert_eq!(s.carry_moments(&aid, &winner).await.unwrap(), 1);
        assert_eq!(
            s.occurrences_of_rule(&winner, rule).await.unwrap(),
            1,
            "the done occurrence stayed behind on the superseded artifact"
        );
        assert_eq!(
            s.occurrences_in_series(&series).await.unwrap(),
            2,
            "the series remembers both"
        );
        assert!(
            !s.rule_is_exhausted(&winner, rule, Some(&series))
                .await
                .unwrap()
        );
        let m = s.moment(&second).await.unwrap().unwrap();
        s.mark_done(&second, 1_200).await.unwrap();
        s.insert_moment(&NewMoment {
            artifact_id: winner.clone(),
            kind: Kind::Due,
            at: Some(173_800),
            tz: "Europe/Berlin".into(),
            rule: Some(rule.into()),
            source: Source::Armed,
            span: None,
            series_id: m.series_id.clone(),
        })
        .await
        .unwrap();
        assert!(
            s.rule_is_exhausted(&winner, rule, Some(&series))
                .await
                .unwrap(),
            "three occurrences is three, wherever the rows ended up"
        );
    }

    /// Completing a reminder retires the note it was read out of. Resolved
    /// through the row's current artifact, a carry across notes made that the
    /// wrong note — one nobody had set a reminder on.
    #[tokio::test]
    async fn a_carried_reminder_still_names_the_note_it_was_read_from() {
        let (s, aid) = store_with_artifact().await;
        let origin = s.get_artifact(&aid).await.unwrap().corpus_id.unwrap();
        let id = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(1_000),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        let winner = other_artifact(&s, "a note nobody set a reminder on").await;
        assert_eq!(s.carry_moments(&aid, &winner).await.unwrap(), 1);

        assert_eq!(
            s.corpus_of_moment(&id).await.unwrap().as_deref(),
            Some(origin.as_str()),
            "the note that was read, not the artifact it sits on today"
        );
        assert!(
            s.has_open_reminder_for_corpus(&origin).await.unwrap(),
            "and the open reminder still counts for that note"
        );
        assert!(s.corpus_was_read_as_reminder(&origin).await.unwrap());
    }

    /// Push a row's `created_at` back before the ladder it is being read
    /// against.
    ///
    /// The ladder only offers a rung the moment was alive for — see
    /// `remind::reachable` — and these tests date their moments in synthetic
    /// seconds well below the wall clock `insert_moment` stamps, so every row
    /// would be one created after its own due time. Backdating is what makes
    /// them the long-standing reminders they are written about.
    async fn existed_since(s: &Store, id: &str, created_at: i64) {
        sqlx::query("UPDATE moments SET created_at = ? WHERE id = ?")
            .bind(created_at)
            .bind(id)
            .execute(&s.pool)
            .await
            .unwrap();
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
            series_id: None,
        }
    }

    /// The search lift and the "1 h ago" badge read this, and it was the one
    /// band read taking `at` raw while every other coalesces the snooze.
    #[tokio::test]
    async fn due_for_reads_the_effective_instant_like_every_other_band_read() {
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.snooze(&id, 8_000).await.unwrap();
        let ids = vec![aid.clone()];
        let got = s.due_for(&ids, 9_000, 10_000).await.unwrap();
        assert_eq!(
            got.get(&aid),
            Some(&8_000),
            "the instant the operator named, not the one they put aside"
        );
    }

    /// `complete_moment` arms a successor only where the artifact does not
    /// already carry that instant, so on an artifact with two same-rule due
    /// rows it arms nothing — and the undo, unfiltered, deleted the other row.
    #[tokio::test]
    async fn undoing_a_completion_takes_only_the_row_it_armed() {
        let (s, aid) = store_with_artifact().await;
        let rule = "FREQ=DAILY";
        let mut read = due(&aid, Some(2_000));
        read.rule = Some(rule.into());
        let read = s.insert_moment(&read).await.unwrap();
        let mut armed = due(&aid, Some(2_000));
        armed.rule = Some(rule.into());
        armed.source = Source::Armed;
        let armed = s.insert_moment(&armed).await.unwrap();

        assert!(s.delete_armed_occurrence(&aid, rule, 2_000).await.unwrap());
        assert!(s.moment(&armed).await.unwrap().is_none());
        assert!(
            s.moment(&read).await.unwrap().is_some(),
            "a row no completion created is not the undo's to take"
        );
        assert!(
            !s.delete_armed_occurrence(&aid, rule, 2_000).await.unwrap(),
            "and there is nothing left that is"
        );
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
        assert!(
            s.open_due(2_000, 10_000).await.unwrap().is_empty(),
            "snoozed past now"
        );
        assert_eq!(
            s.open_due(6_000, 10_000).await.unwrap().len(),
            1,
            "snooze elapsed"
        );
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
        assert_eq!(s.delete_read_due(&aid).await.unwrap(), 1);
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
        existed_since(&s, &a, 0).await;
        existed_since(&s, &b, 0).await;
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            Some(9_000_000 - LEADS[0]),
            "the nearer moment's first rung"
        );
        s.mark_notified(std::slice::from_ref(&b), 9_000_000)
            .await
            .unwrap();
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            Some(10_000_000 - LEADS[0]),
            "b is said and done"
        );
        let owed = s.due_owed(10_000_000 - LEADS[0]).await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].moment.id, a);
        s.snooze(&b, 11_000_000).await.unwrap();
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            Some(10_000_000 - LEADS[0]),
            "b is owed again, after a"
        );
        assert_eq!(s.due_owed(11_000_000).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_row_is_owed_a_push_at_each_rung_of_the_ladder_and_not_between_them() {
        use crate::jobs::remind::LEADS;
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        existed_since(&s, &id, 0).await;
        assert!(
            s.due_owed(at - LEADS[0] - 1).await.unwrap().is_empty(),
            "not yet in the band"
        );

        let owed = s.due_owed(at - LEADS[0]).await.unwrap();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].moment.id, id);
        s.mark_notified(std::slice::from_ref(&id), at - LEADS[0])
            .await
            .unwrap();

        assert!(
            s.due_owed(at - LEADS[1] - 1).await.unwrap().is_empty(),
            "the rung is taken"
        );
        assert_eq!(
            s.due_owed(at - LEADS[1]).await.unwrap().len(),
            1,
            "the next one comes round"
        );
    }

    #[tokio::test]
    async fn the_unit_sleeps_until_the_next_rung_not_until_the_moment() {
        use crate::jobs::remind::LEADS;
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        existed_since(&s, &id, 0).await;
        assert_eq!(s.next_notify_at().await.unwrap(), Some(at - LEADS[0]));
        s.mark_notified(std::slice::from_ref(&id), at - LEADS[0])
            .await
            .unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), Some(at - LEADS[1]));
        s.mark_notified(std::slice::from_ref(&id), at)
            .await
            .unwrap();
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            None,
            "the last rung was the moment itself"
        );
    }

    #[tokio::test]
    async fn a_snooze_is_said_once_when_it_ends_and_not_before() {
        let (s, aid) = store_with_artifact().await;
        let at = 10_000_000;
        let id = s.insert_moment(&due(&aid, Some(at))).await.unwrap();
        s.mark_notified(std::slice::from_ref(&id), at)
            .await
            .unwrap();
        assert_eq!(s.next_notify_at().await.unwrap(), None);

        // An hour, which is shorter than every lead above the last one. On the
        // full ladder the cleared `notified_at` owed one of those rungs at
        // once, and the operator was pushed the reminder they had just put
        // aside, on the next queue tick.
        let until = at + 3_600;
        s.snooze(&id, until).await.unwrap();
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            Some(until),
            "the snooze's own end, and nothing sooner"
        );
        assert!(
            s.due_owed(at + 1).await.unwrap().is_empty(),
            "not the second it was put aside"
        );
        assert!(
            s.due_owed(until - 1).await.unwrap().is_empty(),
            "nor at any lead inside it"
        );

        let owed = s.due_owed(until).await.unwrap();
        assert_eq!(owed.len(), 1, "and then it comes back, once");
        assert_eq!(owed[0].moment.id, id);
        s.mark_notified(std::slice::from_ref(&id), until)
            .await
            .unwrap();
        assert!(
            s.due_owed(until + 10_000).await.unwrap().is_empty(),
            "one rung, said"
        );
        assert_eq!(s.next_notify_at().await.unwrap(), None);
    }

    #[tokio::test]
    async fn due_for_answers_only_the_asked_artifacts_inside_the_window() {
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let hit = s
            .due_for(&[aid.clone(), "other".into()], 500, 2_000)
            .await
            .unwrap();
        assert_eq!(hit.get(&aid), Some(&1_000));
        assert!(
            s.due_for(&["other".into()], 500, 2_000)
                .await
                .unwrap()
                .is_empty(),
            "not asked for"
        );
        assert!(
            s.due_for(std::slice::from_ref(&aid), 500, 900)
                .await
                .unwrap()
                .is_empty(),
            "past the window"
        );
    }

    #[tokio::test]
    async fn due_for_keeps_what_is_already_overdue() {
        // The badge and the lift exist for the reminder you have missed as
        // much as for the one ahead; `due_in` renders "1 h ago" for exactly
        // this row.
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let hit = s
            .due_for(std::slice::from_ref(&aid), 5_000, 9_000)
            .await
            .unwrap();
        assert_eq!(hit.get(&aid), Some(&1_000), "overdue still lifts");
    }

    /// Unlike `due_for`, no horizon: an artifact's own pane asks whether it
    /// carries a reminder at all, not whether one is close enough to belong
    /// on a list.
    #[tokio::test]
    async fn open_due_for_artifact_ignores_the_horizon_but_not_done_or_undated() {
        let (s, aid) = store_with_artifact().await;
        assert!(
            s.open_due_for_artifact(&aid).await.unwrap().is_none(),
            "nothing set yet"
        );

        let far = s.insert_moment(&due(&aid, Some(50_000_000))).await.unwrap();
        let hit = s.open_due_for_artifact(&aid).await.unwrap().unwrap();
        assert_eq!(
            hit.id, far,
            "far outside any horizon, still the pane's own reminder"
        );

        s.mark_done(&far, 900).await.unwrap();
        assert!(
            s.open_due_for_artifact(&aid).await.unwrap().is_none(),
            "done is not open"
        );

        s.insert_moment(&due(&aid, None)).await.unwrap();
        let undated = s.open_due_for_artifact(&aid).await.unwrap().unwrap();
        assert!(
            undated.at.is_none(),
            "an undated reminder is still open, just not lifted by ago_or_ahead"
        );
    }

    #[tokio::test]
    async fn what_the_unit_sleeps_until_is_something_it_will_then_find() {
        // The wake time and the read must agree about the artifact. When they
        // disagree the job wakes, finds nothing owed, re-arms itself at the
        // same past instant, and spins.
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        assert!(s.next_notify_at().await.unwrap().is_some());
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert!(s.due_owed(9_000).await.unwrap().is_empty());
        assert_eq!(
            s.next_notify_at().await.unwrap(),
            None,
            "nothing to find, so nothing to wait for"
        );
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
        s.mark_notified(std::slice::from_ref(&pushed), 10)
            .await
            .unwrap();
        s.snooze(&put_off, 9_000).await.unwrap();

        assert_eq!(
            s.delete_read_due(&aid).await.unwrap(),
            1,
            "only the untouched reading"
        );
        assert!(s.moment(&fresh).await.unwrap().is_none());
        for kept in [&done, &pushed, &put_off] {
            assert!(
                s.moment(kept).await.unwrap().is_some(),
                "a row with a history outlives the reading"
            );
        }
        assert!(
            s.has_moment_at(&aid, Kind::Due, Some(2_000)).await.unwrap(),
            "and the stage knows not to make it twice"
        );
        assert!(!s.has_moment_at(&aid, Kind::Due, Some(1_000)).await.unwrap());
    }

    #[tokio::test]
    async fn a_counted_recurrence_is_exhausted_by_its_rows() {
        let (s, aid) = store_with_artifact().await;
        let mut m = due(&aid, Some(1_000));
        m.rule = Some("FREQ=DAILY;COUNT=2".into());
        assert!(
            !s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2", None)
                .await
                .unwrap(),
            "none yet"
        );
        s.insert_moment(&m).await.unwrap();
        assert!(
            !s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2", None)
                .await
                .unwrap()
        );
        s.insert_moment(&m).await.unwrap();
        assert!(
            s.rule_is_exhausted(&aid, "FREQ=DAILY;COUNT=2", None)
                .await
                .unwrap(),
            "two of two"
        );
        assert!(
            !s.rule_is_exhausted(&aid, "FREQ=DAILY", None).await.unwrap(),
            "open-ended is never exhausted"
        );
    }

    #[tokio::test]
    async fn dating_an_undated_reminder_still_covers_the_undated_probe() {
        // The move off no instant leaves `moved_from` NULL, so the undated
        // probe has to read `moved_at` — or the next re-judgement of the same
        // prose put a second undated row beside the dated one.
        let (s, aid) = store_with_artifact().await;
        let id = s.insert_moment(&due(&aid, None)).await.unwrap();
        s.move_moment(&id, 5_000, "Europe/Berlin").await.unwrap();
        assert!(
            s.has_moment_at(&aid, Kind::Due, None).await.unwrap(),
            "the dated row still answers for the undated reading"
        );
        assert!(s.has_moment_at(&aid, Kind::Due, Some(5_000)).await.unwrap());
    }

    #[tokio::test]
    async fn unsnoozing_does_not_owe_an_already_said_rung_again() {
        // Snoozing used to clear `notified_at`; taking the snooze back then
        // found a bare ladder and the phone said the reminder again.
        let (s, aid) = store_with_artifact().await;
        let now = crate::store::now();
        let id = s.insert_moment(&due(&aid, Some(now - 10))).await.unwrap();
        s.mark_notified(std::slice::from_ref(&id), now - 5)
            .await
            .unwrap();
        assert!(s.due_owed(now).await.unwrap().is_empty(), "said already");

        s.snooze(&id, now + 3_600).await.unwrap();
        s.unsnooze(&id).await.unwrap();
        assert!(
            s.due_owed(now).await.unwrap().is_empty(),
            "an unsnooze owes nothing the push already covered"
        );
        // And a snooze that runs out still re-notifies: the new effective
        // time is ahead of the old mark, so the one-rung ladder owes its end.
        s.snooze(&id, now + 60).await.unwrap();
        assert_eq!(s.due_owed(now + 61).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_elapsed_snooze_re_enters_the_band_at_its_own_time() {
        // Sorted by `m.at`, a row put aside for an hour came back at the top
        // of the band as the most overdue thing on it.
        let (s, aid) = store_with_artifact().await;
        let put_off = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        s.snooze(&put_off, 8_000).await.unwrap();
        let newer = s.insert_moment(&due(&aid, Some(5_000))).await.unwrap();
        let rows = s.open_due(9_000, 20_000).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.moment.id.as_str()).collect();
        assert_eq!(ids, vec![newer.as_str(), put_off.as_str()]);
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
        assert_eq!(
            m.source,
            Source::Cue,
            "moving says nothing about how the date got here"
        );

        assert_eq!(
            s.delete_read_due(&aid).await.unwrap(),
            0,
            "a moved row has a history"
        );
        assert!(
            s.has_moment_at(&aid, Kind::Due, Some(1_000)).await.unwrap(),
            "and the stage will not put a fresh one back on the date that was corrected away from"
        );

        // A second move keeps the first instant: what is worth keeping is what
        // the base read, not the operator's own way to the date they meant.
        s.move_moment(&id, 9_000, "Europe/Berlin").await.unwrap();
        assert_eq!(
            s.moment(&id).await.unwrap().unwrap().moved_from,
            Some(1_000)
        );
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
        assert_eq!(
            s.delete_read_due(&aid).await.unwrap(),
            0,
            "and it is not read away"
        );
    }

    #[tokio::test]
    async fn an_armed_occurrence_is_not_what_the_stage_read() {
        let (s, aid) = store_with_artifact().await;
        let mut m = due(&aid, Some(1_000));
        m.source = Source::Armed;
        let armed = s.insert_moment(&m).await.unwrap();
        let read = s.insert_moment(&due(&aid, Some(2_000))).await.unwrap();
        assert_eq!(
            s.delete_read_due(&aid).await.unwrap(),
            1,
            "only the reading"
        );
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
        s.set_artifact_status(&aid, crate::store::artifacts::ArtifactStatus::Deprecated)
            .await
            .unwrap();
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
