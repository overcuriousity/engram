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
    /// Armed by completing the previous occurrence; not a reading, so `delete_read_due` must
    /// spare it or the recurrence ends at its first completion.
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
    /// `None` also on a row moved off no instant (an undated reminder dated); `moved_at` tells
    /// those apart.
    pub moved_from: Option<i64>,
    pub moved_at: Option<i64>,
    /// The id of the series' first moment (its own id for that one). `None` on a one-shot.
    pub series_id: Option<String>,
    /// The note as it stood when written. `None` on older rows, where the artifact answers.
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
    /// The series a successor continues. A row with a rule and no series heads its own.
    pub series_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DueRow {
    pub moment: Moment,
    pub title: String,
    /// Whether `title` is a written name or the text's opening; see `ui::RowLabel`.
    pub named: bool,
    pub opening: String,
}

#[derive(sqlx::FromRow)]
pub(crate) struct MomentRow {
    pub id: Option<String>,
    pub artifact_id: String,
    pub kind: String,
    pub at: Option<i64>,
    pub until: Option<i64>,
    pub tz: String,
    pub rule: Option<String>,
    pub source: String,
    pub span: Option<String>,
    pub done_at: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub notified_at: Option<i64>,
    pub moved_from: Option<i64>,
    pub moved_at: Option<i64>,
    pub series_id: Option<String>,
    pub origin_corpus_id: Option<String>,
    pub created_at: i64,
}

fn moment_of(r: MomentRow) -> Moment {
    Moment {
        id: r.id.unwrap_or_default(),
        artifact_id: r.artifact_id,
        kind: Kind::parse(r.kind.as_str()).unwrap_or(Kind::Event),
        at: r.at,
        until: r.until,
        tz: r.tz,
        rule: r.rule,
        source: Source::parse(r.source.as_str()).unwrap_or(Source::Extracted),
        span: r.span,
        done_at: r.done_at,
        snoozed_until: r.snoozed_until,
        notified_at: r.notified_at,
        moved_from: r.moved_from,
        moved_at: r.moved_at,
        series_id: r.series_id,
        origin_corpus_id: r.origin_corpus_id,
        created_at: r.created_at,
    }
}

/// A moment joined with its artifact's title, text and provenance, for `query_as!`.
#[derive(sqlx::FromRow)]
pub(crate) struct DueSql {
    pub id: Option<String>,
    pub artifact_id: String,
    pub kind: String,
    pub at: Option<i64>,
    pub until: Option<i64>,
    pub tz: String,
    pub rule: Option<String>,
    pub source: String,
    pub span: Option<String>,
    pub done_at: Option<i64>,
    pub snoozed_until: Option<i64>,
    pub notified_at: Option<i64>,
    pub moved_from: Option<i64>,
    pub moved_at: Option<i64>,
    pub series_id: Option<String>,
    pub origin_corpus_id: Option<String>,
    pub created_at: i64,
    pub title: Option<String>,
    pub text: String,
    pub provenance: String,
}

fn row_of(r: DueSql) -> Result<DueRow> {
    let opening = r
        .text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect::<String>();
    // Neither a passage's heading nor a note is a name for this text; see
    // `Provenance::names_its_own_text`.
    let named = crate::store::artifacts::Provenance::parse(&r.provenance).names_its_own_text();
    let title = r.title.filter(|t| !t.is_empty() && named);
    let moment = moment_of(MomentRow {
        id: r.id,
        artifact_id: r.artifact_id,
        kind: r.kind,
        at: r.at,
        until: r.until,
        tz: r.tz,
        rule: r.rule,
        source: r.source,
        span: r.span,
        done_at: r.done_at,
        snoozed_until: r.snoozed_until,
        notified_at: r.notified_at,
        moved_from: r.moved_from,
        moved_at: r.moved_at,
        series_id: r.series_id,
        origin_corpus_id: r.origin_corpus_id,
        created_at: r.created_at,
    });
    Ok(DueRow {
        moment,
        named: title.is_some(),
        title: title.unwrap_or_else(|| opening.clone()),
        opening,
    })
}

/// An elapsed snooze is the row's time, which is why ladder reads coalesce it over `at`.
fn eff_at(r: &DueRow) -> i64 {
    r.moment.snoozed_until.or(r.moment.at).unwrap_or(0)
}

impl Store {
    /// `origin_corpus_id` first: `carry_moments` may have moved the row onto an artifact of
    /// another note, which `complete_moment` would then wrongly retire.
    pub async fn corpus_of_moment(&self, moment_id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT COALESCE(m.origin_corpus_id, a.corpus_id) \
             FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE m.id = ?",
            moment_id
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// `kind = 'due'`: an event is never completed and would hold the note open forever.
    /// `a.status = 'active'`, as every listing says: an invisible row cannot be completed.
    pub async fn has_open_reminder_for_corpus(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE COALESCE(m.origin_corpus_id, a.corpus_id) = ? AND m.kind = 'due' \
               AND m.done_at IS NULL AND a.status = 'active'",
            corpus_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// `cue` and `classified` are readings, `set` a person, `extracted` a passing date.
    /// `move_moment` leaves `source` alone, so a moved reminder still counts as read.
    pub async fn corpus_was_read_as_reminder(&self, corpus_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments m JOIN artifacts a ON a.id = m.artifact_id \
             WHERE COALESCE(m.origin_corpus_id, a.corpus_id) = ? \
               AND m.source IN ('cue', 'classified')",
            corpus_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Fills `series_id` (own id for a head with a rule) and `origin_corpus_id`, which
    /// `carry_moments` would make underivable. For a merge, the lineage's note if it names one.
    pub async fn insert_moment(&self, m: &NewMoment) -> Result<String> {
        let id = new_id();
        let series: Option<String> = match (&m.series_id, &m.rule) {
            (Some(s), _) => Some(s.clone()),
            (None, Some(_)) => Some(id.clone()),
            (None, None) => None,
        };
        let corpus: Option<String> = match sqlx::query_scalar!(
            "SELECT corpus_id FROM artifacts WHERE id = ?",
            &m.artifact_id
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten()
        {
            Some(c) => Some(c),
            // `DISTINCT ... LIMIT 2`: one row is the note, two means several.
            None => {
                let roots: Vec<String> = sqlx::query_scalar!(
                    r#"SELECT DISTINCT a.corpus_id AS "corpus_id!" FROM artifact_sources s
                       JOIN artifacts a ON a.id = s.root_id
                      WHERE s.child_id = ? AND a.corpus_id IS NOT NULL
                      LIMIT 2"#,
                    &m.artifact_id
                )
                .fetch_all(&self.pool)
                .await?;
                match roots.len() {
                    1 => roots.into_iter().next(),
                    _ => None,
                }
            }
        };
        sqlx::query!(
            "INSERT INTO moments (id, artifact_id, kind, at, tz, rule, source, span,
                                  series_id, origin_corpus_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &id,
            &m.artifact_id,
            m.kind.as_str(),
            m.at,
            &m.tz,
            &m.rule,
            m.source.as_str(),
            &m.span,
            &series,
            &corpus,
            crate::store::now()
        )
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// `IS`, not `=`, so an undated reminder matches. `moved_from` stops a re-read restoring a
    /// corrected date; `moved_at` covers the undated probe of a row moved off no instant.
    pub async fn has_moment_at(
        &self,
        artifact_id: &str,
        kind: Kind,
        at: Option<i64>,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ?
                AND (at IS ?
                     OR (moved_from IS NOT NULL AND moved_from IS ?)
                     OR (? IS NULL AND moved_at IS NOT NULL AND moved_from IS NULL))",
            artifact_id,
            kind.as_str(),
            at,
            at,
            at
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Open rows only, and never onto an instant the winner already holds open under the same
    /// rule: done rows are history, and a one-shot and a recurrence at one instant are two.
    pub async fn carry_moments(&self, loser: &str, winner: &str) -> Result<u64> {
        if loser == winner {
            return Ok(0);
        }
        let res = sqlx::query!(
            "UPDATE moments SET artifact_id = ?
              WHERE artifact_id = ? AND done_at IS NULL
                AND NOT EXISTS (SELECT 1 FROM moments w
                                 WHERE w.artifact_id = ?
                                   AND w.kind = moments.kind
                                   AND w.at IS moments.at
                                   AND w.rule IS moments.rule
                                   AND w.done_at IS NULL)",
            winner,
            loser,
            winner
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// A hand-moved row wins, most recent first; else the series head, before DST rolled it.
    /// `None` leaves `next_after` reading the previous occurrence.
    pub async fn series_anchor(&self, series_id: &str) -> Result<Option<i64>> {
        let moved: Option<i64> = sqlx::query_scalar!(
            "SELECT at FROM moments
              WHERE series_id = ? AND at IS NOT NULL AND moved_at IS NOT NULL
              ORDER BY moved_at DESC LIMIT 1",
            series_id
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        if moved.is_some() {
            return Ok(moved);
        }
        Ok(sqlx::query_scalar!(
            "SELECT at FROM moments
              WHERE series_id = ? AND at IS NOT NULL
              ORDER BY created_at, id LIMIT 1",
            series_id
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Narrower than `has_moment_at`: a row moved off this instant no longer speaks for it,
    /// or a hand-moved daily reminder never arms its successor.
    pub async fn moment_parked_at(
        &self,
        artifact_id: &str,
        kind: Kind,
        at: Option<i64>,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ? AND at IS ?",
            artifact_id,
            kind.as_str(),
            at
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// A row the base has already pushed or snoozed is not a re-read's to replace.
    pub async fn has_acted_on_due(&self, artifact_id: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL
                AND (notified_at IS NOT NULL OR snoozed_until IS NOT NULL)",
            artifact_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// A re-read can add a rule to the row at its instant; the row then heads its own series.
    pub async fn set_rule_of_open_due(
        &self,
        artifact_id: &str,
        at: Option<i64>,
        rule: &str,
    ) -> Result<bool> {
        // Not on a row a person touched: a rule they removed must not be read back.
        Ok(sqlx::query!(
            "UPDATE moments SET rule = ?, series_id = COALESCE(series_id, id)
              WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL AND at IS ?
                AND (rule IS NULL OR rule <> ?)
                AND moved_at IS NULL AND source <> 'set'",
            rule,
            artifact_id,
            at,
            rule
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// A person's date or hand-made reminder outranks a re-read landing on another instant.
    /// Only the read is refused: completion still arms the next occurrence.
    pub async fn has_operator_moment(&self, artifact_id: &str, kind: Kind) -> Result<bool> {
        let n: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments
              WHERE artifact_id = ? AND kind = ? AND (moved_at IS NOT NULL OR source = 'set')",
            artifact_id,
            kind.as_str()
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Done rows included: the rows are the occurrences.
    pub async fn occurrences_of_rule(&self, artifact_id: &str, rule: &str) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments WHERE artifact_id = ? AND kind = 'due' AND rule = ?",
            artifact_id,
            rule
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Also adopts the earlier rows of the same rule, so a `COUNT` spanning pre-column rows
    /// counts them. Safe to call on a row that already has a series.
    pub async fn adopt_into_own_series(&self, id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE moments SET series_id = ?
              WHERE series_id IS NULL
                AND (id = ?
                     OR (kind = 'due'
                         AND artifact_id = (SELECT artifact_id FROM moments
                                             WHERE id = ? AND series_id IS NULL)
                         AND rule = (SELECT rule FROM moments
                                      WHERE id = ? AND series_id IS NULL)))",
            id,
            id,
            id,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Immune to supersession: `carry_moments` moves only the open row, so per-artifact counts
    /// would re-arm past `COUNT`.
    pub async fn occurrences_in_series(&self, series_id: &str) -> Result<i64> {
        Ok(sqlx::query_scalar!(
            "SELECT COUNT(*) FROM moments WHERE kind = 'due' AND series_id = ?",
            series_id
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// False for no COUNT and for an unparsable rule (`next_after` refuses those). Counted
    /// over the series where the row has one, else the artifact's rows.
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
        Ok(sqlx::query_as!(
            crate::store::moments::MomentRow,
            "SELECT * FROM moments WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await?
        .map(moment_of))
    }

    /// Not bounded to a horizon: the pane reports a fact about the artifact.
    pub async fn open_due_for_artifact(&self, artifact_id: &str) -> Result<Option<Moment>> {
        // Ordered on the snoozed instant, as `due_for` and `open_due` order.
        Ok(sqlx::query_as!(
            MomentRow,
            "SELECT * FROM moments WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL
             ORDER BY COALESCE(snoozed_until, at) IS NULL, COALESCE(snoozed_until, at) LIMIT 1",
            artifact_id
        )
        .fetch_optional(&self.pool)
        .await?
        .map(moment_of))
    }

    /// Separate from `delete_read_due` for timing: whether a note is a reminder is settled later,
    /// past returns that file nothing. See `jobs::judgement::apply`.
    pub async fn delete_read_events(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'event' AND source NOT IN ('set', 'armed')
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL
                AND moved_at IS NULL",
            artifact_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Spares `set` and `armed` rows and any with a history (`done_at`, `notified_at`,
    /// `snoozed_until`, `moved_at`): every embed re-arms this stage.
    pub async fn delete_read_due(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND source NOT IN ('set', 'armed')
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL
                AND moved_at IS NULL",
            artifact_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// The operator's button: removes every stage-made row including `armed`, history or not.
    /// `set` and done rows stay. Returns whether anything went.
    pub async fn delete_refused_due(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND source <> 'set' AND done_at IS NULL",
            artifact_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// The horizon and order use the effective (snoozed) instant, as `uncovered` and
    /// `next_notify_at` push on it.
    pub async fn open_due(&self, now: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query_as!(

    DueSql,
    r#"SELECT m.id AS "id?", m.artifact_id AS "artifact_id!", m.kind AS "kind!", m.at AS "at?", m.until AS "until?", m.tz AS "tz!", m.rule AS "rule?", m.source AS "source!", m.span AS "span?", m.done_at AS "done_at?", m.snoozed_until AS "snoozed_until?", m.notified_at AS "notified_at?", m.moved_from AS "moved_from?", m.moved_at AS "moved_at?", m.series_id AS "series_id?", m.origin_corpus_id AS "origin_corpus_id?", m.created_at AS "created_at!", a.title AS "title?", a.text AS "text!", a.provenance AS "provenance!" FROM moments m JOIN artifacts a ON a.id = m.artifact_id WHERE m.kind = 'due' AND m.done_at IS NULL AND a.status = 'active'
               AND (m.at IS NULL
                    OR (COALESCE(m.snoozed_until, m.at) < ?
                        AND (m.snoozed_until IS NULL OR m.snoozed_until <= ?)))
             ORDER BY m.at IS NULL, COALESCE(m.snoozed_until, m.at), m.created_at"#,
            to,
            now
        ).fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_of).collect()
    }

    /// No lower bound: an overdue reminder most deserves the badge.
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
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            // The effective instant, for the horizon too; see `eff_at`.
            "SELECT artifact_id, MIN(COALESCE(snoozed_until, at)) AS at FROM moments
             WHERE kind = 'due' AND done_at IS NULL AND at IS NOT NULL
               AND COALESCE(snoozed_until, at) < ?
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
        let rows = sqlx::query_as!(

    DueSql,
    r#"SELECT m.id AS "id?", m.artifact_id AS "artifact_id!", m.kind AS "kind!", m.at AS "at?", m.until AS "until?", m.tz AS "tz!", m.rule AS "rule?", m.source AS "source!", m.span AS "span?", m.done_at AS "done_at?", m.snoozed_until AS "snoozed_until?", m.notified_at AS "notified_at?", m.moved_from AS "moved_from?", m.moved_at AS "moved_at?", m.series_id AS "series_id?", m.origin_corpus_id AS "origin_corpus_id?", m.created_at AS "created_at!", a.title AS "title?", a.text AS "text!", a.provenance AS "provenance!" FROM moments m JOIN artifacts a ON a.id = m.artifact_id WHERE m.kind = 'event' AND a.status = 'active' AND m.at >= ? AND m.at < ? ORDER BY m.at"#,
            from,
            to
        ).fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_of).collect()
    }

    /// Done rows stay listed whatever their artifact's status (`carry_moments` moves only open
    /// rows); open rows are filtered by status, or a carried moment lists twice.
    pub async fn moments_between(&self, from: i64, to: i64) -> Result<Vec<DueRow>> {
        let rows = sqlx::query_as!(

    DueSql,
    r#"SELECT m.id AS "id?", m.artifact_id AS "artifact_id!", m.kind AS "kind!", m.at AS "at?", m.until AS "until?", m.tz AS "tz!", m.rule AS "rule?", m.source AS "source!", m.span AS "span?", m.done_at AS "done_at?", m.snoozed_until AS "snoozed_until?", m.notified_at AS "notified_at?", m.moved_from AS "moved_from?", m.moved_at AS "moved_at?", m.series_id AS "series_id?", m.origin_corpus_id AS "origin_corpus_id?", m.created_at AS "created_at!", a.title AS "title?", a.text AS "text!", a.provenance AS "provenance!" FROM moments m JOIN artifacts a ON a.id = m.artifact_id WHERE (a.status = 'active' OR m.done_at IS NOT NULL) AND a.status <> 'trashed'  AND m.at >= ? AND m.at < ? ORDER BY m.at"#,
            from,
            to
        ).fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_of).collect()
    }

    /// `done_at IS NULL` stops two concurrent presses both arming a successor.
    pub async fn mark_done(&self, id: &str, at: i64) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE moments SET done_at = ? WHERE id = ? AND done_at IS NULL",
            at,
            id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    pub async fn undo_done(&self, id: &str) -> Result<()> {
        sqlx::query!("UPDATE moments SET done_at = NULL WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// An UPDATE, so a recurrence keeps one row per firing. `source` is left alone; `COALESCE`
    /// keeps the first `moved_from`. The snooze and notification go with the old date.
    pub async fn move_moment(&self, id: &str, at: i64, tz: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE moments
                SET moved_from = COALESCE(moved_from, at), moved_at = ?, at = ?, tz = ?,
                    snoozed_until = NULL, notified_at = NULL
              WHERE id = ?",
            crate::store::now(),
            at,
            tz,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The row heads a new series (or none); `moved_at` stops re-reads restoring the old rule.
    pub async fn restate_rule(&self, id: &str, rule: Option<&str>) -> Result<bool> {
        Ok(sqlx::query!(
            "UPDATE moments
                SET rule = ?1, series_id = CASE WHEN ?1 IS NULL THEN NULL ELSE id END,
                    moved_at = ?2
              WHERE id = ?3 AND kind = 'due' AND done_at IS NULL",
            rule,
            crate::store::now(),
            id
        )
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Unlike `delete_refused_due`, spares nothing: the operator is taking back their own.
    pub async fn delete_open_due(&self, artifact_id: &str) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM moments WHERE artifact_id = ? AND kind = 'due' AND done_at IS NULL",
            artifact_id
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// `source = 'armed'` and untouched only: a row this completion did not create, or with a
    /// history of its own, is not the undo's to delete.
    pub async fn delete_armed_occurrence(
        &self,
        artifact_id: &str,
        rule: &str,
        at: i64,
    ) -> Result<bool> {
        let n = sqlx::query!(
            "DELETE FROM moments
              WHERE artifact_id = ? AND kind = 'due' AND rule = ? AND at = ?
                AND source = 'armed'
                AND done_at IS NULL AND notified_at IS NULL AND snoozed_until IS NULL",
            artifact_id,
            rule,
            at
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Returns whether a row was put aside. `notified_at` is kept: the snooze's end re-notifies
    /// anyway (see `owed_lead`). Undated rows refuse: `open_due` always lists them.
    pub async fn snooze(&self, id: &str, until: i64) -> Result<bool> {
        Ok(
            sqlx::query!(
                "UPDATE moments SET snoozed_until = ?                  WHERE id = ? AND done_at IS NULL AND at IS NOT NULL",
                until,
                id
            )
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    pub async fn unsnooze(&self, id: &str) -> Result<()> {
        sqlx::query!("UPDATE moments SET snoozed_until = NULL WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One statement: the push already covered every id, and a partial mark re-pushes the rest.
    pub async fn mark_notified(&self, ids: &[String], at: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let marks = vec!["?"; ids.len()].join(",");
        // runtime: one placeholder per item in the IN list.
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

    /// Loose read; the ladder is applied in Rust over `owed_lead` so the rungs live in one place.
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

    async fn uncovered(&self) -> Result<Vec<DueRow>> {
        let rows = sqlx::query_as!(

    DueSql,
    r#"SELECT m.id AS "id?", m.artifact_id AS "artifact_id!", m.kind AS "kind!", m.at AS "at?", m.until AS "until?", m.tz AS "tz!", m.rule AS "rule?", m.source AS "source!", m.span AS "span?", m.done_at AS "done_at?", m.snoozed_until AS "snoozed_until?", m.notified_at AS "notified_at?", m.moved_from AS "moved_from?", m.moved_at AS "moved_at?", m.series_id AS "series_id?", m.origin_corpus_id AS "origin_corpus_id?", m.created_at AS "created_at!", a.title AS "title?", a.text AS "text!", a.provenance AS "provenance!" FROM moments m JOIN artifacts a ON a.id = m.artifact_id WHERE m.kind = 'due' AND m.done_at IS NULL AND m.at IS NOT NULL
               AND a.status = 'active'
               AND (m.notified_at IS NULL OR m.notified_at < COALESCE(m.snoozed_until, m.at))
             ORDER BY COALESCE(m.snoozed_until, m.at)"#
        ).fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_of).collect()
    }

    /// Called by every write that can move the earliest owed moment.
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

    /// The next second the band's contents change: `at - horizon`, `at`, or `snoozed_until`.
    pub async fn next_due_change(&self, now: i64, horizon: i64) -> Result<Option<i64>> {
        let r: Option<Option<i64>> = sqlx::query_scalar!(
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
            horizon,
            now
        )
            .fetch_optional(&self.pool)
        .await?;
        Ok(r.flatten())
    }

    /// A rung already behind us is returned as is, so the unit fires at once.
    pub async fn next_notify_at(&self) -> Result<Option<i64>> {
        // The same read as `due_owed`, or a deprecated artifact's moment makes the unit spin.
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
                    text: "Remind me friday to send the invoice".into(),
                    title: Some("Invoice".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        (s, made[0].id.clone())
    }

    #[tokio::test]
    async fn a_reminder_born_on_a_merge_of_one_note_still_names_that_note() {
        let s = Store::memory().await.unwrap();
        let c = s.insert_corpus("the note", "ui", None).await.unwrap();
        let made = s
            .insert_artifacts(&c.id, &[na(0, "first half"), na(1, "second half")])
            .await
            .unwrap();
        let merged = s
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "both halves".into(),
                    title: Some("merged".into()),
                    ..Default::default()
                },
                &[made[0].id.clone(), made[1].id.clone()],
            )
            .await
            .unwrap()
            .id;
        assert!(s.get_artifact(&merged).await.unwrap().corpus_id.is_none());

        let id = s.insert_moment(&due_on(&merged)).await.unwrap();
        assert_eq!(
            s.corpus_of_moment(&id).await.unwrap().as_deref(),
            Some(c.id.as_str()),
            "one note behind the merge is the note the reminder is about"
        );
        assert!(s.has_open_reminder_for_corpus(&c.id).await.unwrap());
    }

    #[tokio::test]
    async fn a_reminder_born_on_a_merge_of_two_notes_names_neither() {
        let s = Store::memory().await.unwrap();
        let a = s.insert_corpus("one note", "ui", None).await.unwrap();
        let b = s.insert_corpus("another note", "ui", None).await.unwrap();
        let one = s.insert_artifacts(&a.id, &[na(0, "one")]).await.unwrap();
        let two = s.insert_artifacts(&b.id, &[na(0, "two")]).await.unwrap();
        let merged = s
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "both notes".into(),
                    title: Some("merged".into()),
                    ..Default::default()
                },
                &[one[0].id.clone(), two[0].id.clone()],
            )
            .await
            .unwrap()
            .id;

        let id = s.insert_moment(&due_on(&merged)).await.unwrap();
        assert_eq!(s.corpus_of_moment(&id).await.unwrap(), None);
    }

    fn na(ordinal: i64, text: &str) -> NewArtifact {
        NewArtifact {
            ordinal,
            text: text.into(),
            title: Some(text.into()),
            ..Default::default()
        }
    }

    fn due_on(artifact_id: &str) -> NewMoment {
        NewMoment {
            artifact_id: artifact_id.to_string(),
            kind: Kind::Due,
            at: Some(crate::store::now() + 3_600),
            tz: "UTC".into(),
            rule: None,
            source: Source::Set,
            span: None,
            series_id: None,
        }
    }

    async fn other_artifact(s: &Store, text: &str) -> String {
        let c = s.insert_corpus(text, "ui", None).await.unwrap();
        let made = s
            .insert_artifacts(
                &c.id,
                &[NewArtifact {
                    text: text.into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        made[0].id.clone()
    }

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

    #[tokio::test]
    async fn a_done_row_on_the_winner_at_the_same_instant_does_not_block_the_carry() {
        let (s, aid) = store_with_artifact().await;
        let winner = other_artifact(&s, "the promoted rewrite").await;
        let at = Some(1_000);
        let row = |artifact: &str| NewMoment {
            artifact_id: artifact.to_string(),
            kind: Kind::Due,
            at,
            tz: "Europe/Berlin".into(),
            rule: None,
            source: Source::Classified,
            span: None,
            series_id: None,
        };
        let finished = s.insert_moment(&row(&winner)).await.unwrap();
        s.mark_done(&finished, 1_100).await.unwrap();
        let open = s.insert_moment(&row(&aid)).await.unwrap();

        assert_eq!(
            s.carry_moments(&aid, &winner).await.unwrap(),
            1,
            "an open row is carried past a done one at the same instant"
        );
        assert_eq!(
            s.moment(&open).await.unwrap().unwrap().artifact_id,
            winner,
            "and it is the winner that carries it now"
        );
    }

    #[tokio::test]
    async fn an_open_row_on_the_winner_at_the_same_instant_still_blocks_the_carry() {
        let (s, aid) = store_with_artifact().await;
        let winner = other_artifact(&s, "the promoted rewrite").await;
        let at = Some(1_000);
        let row = |artifact: &str| NewMoment {
            artifact_id: artifact.to_string(),
            kind: Kind::Due,
            at,
            tz: "Europe/Berlin".into(),
            rule: None,
            source: Source::Classified,
            span: None,
            series_id: None,
        };
        s.insert_moment(&row(&winner)).await.unwrap();
        let loser_row = s.insert_moment(&row(&aid)).await.unwrap();

        assert_eq!(s.carry_moments(&aid, &winner).await.unwrap(), 0);
        assert_eq!(
            s.moment(&loser_row).await.unwrap().unwrap().artifact_id,
            aid,
            "not moved, and not doubled on the winner"
        );
    }

    #[tokio::test]
    async fn a_recurrence_is_carried_past_a_one_shot_standing_at_the_same_instant() {
        let (s, aid) = store_with_artifact().await;
        let winner = other_artifact(&s, "the promoted rewrite").await;
        let at = Some(1_000);
        let row = |artifact: &str, rule: Option<&str>| NewMoment {
            artifact_id: artifact.to_string(),
            kind: Kind::Due,
            at,
            tz: "Europe/Berlin".into(),
            rule: rule.map(str::to_string),
            source: Source::Classified,
            span: None,
            series_id: None,
        };
        s.insert_moment(&row(&winner, None)).await.unwrap();
        let weekly = s
            .insert_moment(&row(&aid, Some("FREQ=WEEKLY")))
            .await
            .unwrap();

        assert_eq!(s.carry_moments(&aid, &winner).await.unwrap(), 1);
        assert_eq!(
            s.moment(&weekly).await.unwrap().unwrap().artifact_id,
            winner,
            "every Tuesday at nine is not the same reminder as on Tuesday at nine"
        );
    }

    #[tokio::test]
    async fn a_reminder_snoozed_into_the_horizon_from_beyond_it_is_on_the_band() {
        let (s, aid) = store_with_artifact().await;
        let now = 1_000_000;
        let horizon = now + 48 * 3_600;
        let id = s
            .insert_moment(&NewMoment {
                artifact_id: aid.clone(),
                kind: Kind::Due,
                at: Some(now + 3 * 86_400),
                tz: "Europe/Berlin".into(),
                rule: None,
                source: Source::Classified,
                span: None,
                series_id: None,
            })
            .await
            .unwrap();
        assert!(
            s.open_due(now, horizon).await.unwrap().is_empty(),
            "three days out is past the horizon, as it should be"
        );

        s.snooze(&id, now + 3_600).await.unwrap();
        assert!(
            s.open_due(now, horizon).await.unwrap().is_empty(),
            "and a snooze that has not elapsed is still put aside"
        );

        let after = now + 2 * 3_600;
        let rows = s.open_due(after, after + 48 * 3_600).await.unwrap();
        assert_eq!(rows.len(), 1, "the hour passed; it is due now");
        assert_eq!(rows[0].moment.id, id);
        let lift = s
            .due_for(std::slice::from_ref(&aid), after, after + 48 * 3_600)
            .await
            .unwrap();
        assert_eq!(
            lift.get(&aid).copied(),
            Some(now + 3_600),
            "and the badge reads the instant it was snoozed to"
        );
    }

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

    async fn existed_since(s: &Store, id: &str, created_at: i64) {
        sqlx::query!(
            "UPDATE moments SET created_at = ? WHERE id = ?",
            created_at,
            id
        )
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

        // Shorter than every lead but the last, so a cleared `notified_at` would push at once.
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
        let (s, aid) = store_with_artifact().await;
        s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let hit = s
            .due_for(std::slice::from_ref(&aid), 5_000, 9_000)
            .await
            .unwrap();
        assert_eq!(hit.get(&aid), Some(&1_000), "overdue still lifts");
    }

    #[tokio::test]
    async fn open_due_for_artifact_reads_a_snooze_as_the_rows_own_instant() {
        // The pane renders `snoozed_until.or(at)`, so ordering must use the same instant.
        let (s, aid) = store_with_artifact().await;
        let monday = s.insert_moment(&due(&aid, Some(1_000))).await.unwrap();
        let wednesday = s.insert_moment(&due(&aid, Some(3_000))).await.unwrap();
        assert_eq!(
            s.open_due_for_artifact(&aid).await.unwrap().unwrap().id,
            monday,
            "the earlier row, while nothing is put aside"
        );

        assert!(s.snooze(&monday, 10_000).await.unwrap());
        let hit = s.open_due_for_artifact(&aid).await.unwrap().unwrap();
        assert_eq!(
            hit.id, wednesday,
            "a snooze past the other row hands the pane that row instead"
        );
    }

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
        // If the wake time and the read disagree, the job wakes, finds nothing and spins.
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
        // Every embed re-arms the moments stage; a finished or pushed reminder must not return.
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
        s.snooze(&id, now + 60).await.unwrap();
        assert_eq!(s.due_owed(now + 61).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_elapsed_snooze_re_enters_the_band_at_its_own_time() {
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

        s.move_moment(&id, 9_000, "Europe/Berlin").await.unwrap();
        assert_eq!(
            s.moment(&id).await.unwrap().unwrap().moved_from,
            Some(1_000)
        );
    }

    #[tokio::test]
    async fn dating_an_undated_reminder_is_a_move_with_no_instant_to_keep() {
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
