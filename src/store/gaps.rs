//! The holes, as rows: unanswered questions and gap searches, and the groups
//! the sweep made of them.

use super::{Store, now};
use crate::error::{Error, Result};
use crate::store::feedback::blob_to_vec;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GapKind {
    Ask,
    Search,
    /// Every candidate under `vector.weak_below`. Distance, not behaviour: an unopened result
    /// is ambiguous, and opens are only recorded when pursuits and feedback are on.
    Unmatched,
    /// A pursuit closed `unsatisfied`. One written without a vector is not a gap (`vec_dim > 0`).
    Pursuit,
    /// A subject `[infer.ask] plan` named as missing whose fan-out found nothing near it, by
    /// `Unmatched`'s measure. Web door only: it rides with `record_ask`'s consent line.
    Subject,
}

impl GapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Ask => "ask",
            GapKind::Search => "search",
            GapKind::Unmatched => "unmatched",
            GapKind::Pursuit => "pursuit",
            GapKind::Subject => "subject",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(GapKind::Ask),
            "search" => Some(GapKind::Search),
            "unmatched" => Some(GapKind::Unmatched),
            "pursuit" => Some(GapKind::Pursuit),
            "subject" => Some(GapKind::Subject),
            _ => None,
        }
    }
}

/// `Distance` is a measurement, dropped by `trim_gap_coverage` once the line moves above it;
/// `Capture` is the operator answering in the same box, which no line can contradict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoveredBy {
    Distance,
    Capture,
}

impl CoveredBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CoveredBy::Distance => "distance",
            CoveredBy::Capture => "capture",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Gap {
    pub kind: GapKind,
    pub id: String,
    pub text: String,
}

/// The display path reads `Gap` alone: vectors are the expensive half of the row.
#[derive(Debug, Clone)]
pub struct GapVec {
    pub gap: Gap,
    pub vec: Vec<f32>,
}

/// `capped` matters: the sweep deletes clusters it did not see, and a capped pass misses some.
pub struct OpenGaps {
    pub gaps: Vec<GapVec>,
    pub capped: bool,
}

#[derive(Debug, Clone)]
pub struct GapCluster {
    pub key: String,
    pub label: String,
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCluster {
    pub key: String,
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

#[derive(Debug, Clone)]
pub struct GapRow {
    pub label: String,
    pub labelled_by: String,
    pub members: Vec<Gap>,
}

/// Per kind (five kinds): the sweep's clustering is quadratic in the total and the capture
/// page renders every row. Newest first, ties broken by uuid v7 id; `open_gaps` logs a bite.
pub const MAX_OPEN_GAPS: i64 = 500;

/// One `WHERE` for three projections, so page, sweep and count agree on what is open.
macro_rules! ask_gaps_from {
    () => {
        " FROM ask_events
          WHERE verdict = 'nothing_here' AND dismissed_at IS NULL
            AND embed_model = ? AND vec_dim > 0
            AND NOT EXISTS (SELECT 1 FROM gap_coverage
                             WHERE kind = 'ask' AND gap_id = ask_events.id)"
    };
}

macro_rules! ask_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, question AS text",
            $cols,
            ask_gaps_from!(),
            " ORDER BY judged_at DESC, id DESC LIMIT ?"
        )
    };
}

/// `search` and `unmatched` are two readings of one row, so coverage of either closes it.
macro_rules! search_gaps_from {
    () => {
        " FROM search_events
          WHERE verdict = 'gap' AND dismissed_at IS NULL
            AND embed_model = ? AND vec_dim > 0
            AND NOT EXISTS (SELECT 1 FROM gap_coverage
                             WHERE kind IN ('search', 'unmatched')
                               AND gap_id = search_events.id)"
    };
}

macro_rules! search_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, query AS text",
            $cols,
            search_gaps_from!(),
            " ORDER BY judged_at DESC, id DESC LIMIT ?"
        )
    };
}

/// `MAX(similarity)` over no rows is NULL: a search with no candidates is named explicitly,
/// unscored candidates (`similarity IS NULL`) are not a gap, judged searches are excluded,
/// and `length >= 3` matches `dealable!` so the two predicates are exact complements.
macro_rules! unmatched_gaps_from {
    () => {
        " FROM search_events e
          WHERE e.dismissed_at IS NULL AND e.embed_model = ? AND e.vec_dim > 0
            AND e.verdict IS NULL AND length(e.query) >= 3
            AND NOT EXISTS (SELECT 1 FROM gap_coverage
                             WHERE kind = 'unmatched' AND gap_id = e.id)
            AND (NOT EXISTS (SELECT 1 FROM search_candidates c WHERE c.event_id = e.id)
                 OR (SELECT MAX(c.similarity) FROM search_candidates c
                      WHERE c.event_id = e.id AND c.similarity IS NOT NULL) < ?)"
    };
}

macro_rules! unmatched_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT e.id, e.query AS text",
            $cols,
            unmatched_gaps_from!(),
            " ORDER BY e.created_at DESC, e.id DESC LIMIT ?"
        )
    };
}

/// The join to `ask_events` makes the row die with its question.
macro_rules! subject_gaps_from {
    () => {
        " FROM ask_subjects s
          JOIN ask_events e ON e.id = s.event_id
          WHERE s.dismissed_at IS NULL
            AND s.embed_model = ? AND s.vec_dim > 0
            AND NOT EXISTS (SELECT 1 FROM gap_coverage
                             WHERE kind = 'subject' AND gap_id = s.id)"
    };
}

macro_rules! subject_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT s.id, s.subject AS text",
            $cols,
            subject_gaps_from!(),
            " ORDER BY s.created_at DESC, s.id DESC LIMIT ?"
        )
    };
}

/// Written once so the page and its count agree on "still unsatisfied".
macro_rules! pursuit_gaps_from {
    () => {
        " FROM pursuits
          WHERE state = 'unsatisfied' AND embed_model = ? AND vec_dim > 0
            AND NOT EXISTS (SELECT 1 FROM gap_coverage
                             WHERE kind = 'pursuit' AND gap_id = pursuits.id)"
    };
}

macro_rules! pursuit_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, queries",
            $cols,
            pursuit_gaps_from!(),
            " ORDER BY opened_at DESC, id DESC LIMIT ?"
        )
    };
}

/// The leading query only: a member that is a paragraph would crowd out the naming prompt.
fn pursuit_text(queries_json: &str) -> String {
    serde_json::from_str::<Vec<String>>(queries_json)
        .ok()
        .and_then(|q| q.into_iter().next())
        .unwrap_or_default()
}

/// Every recorded search and question, not only gaps: the unrelated line must be measured
/// across topics. 200 a side is 79,800 pairs.
pub const CALIBRATION_SAMPLE: i64 = 200;

impl Store {
    /// Matches on `queries` with `normalize_query`, since pursuits store no event ids. Every
    /// query, not only the leading one: a burst can leave a half-typed prefix leading.
    pub async fn pursuit_gaps_of_query(
        &self,
        embed_model: &str,
        query: &str,
    ) -> Result<Vec<String>> {
        let want = crate::store::links::normalize_query(query);
        if want.is_empty() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(pursuit_gaps_sql!(""))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS)
            .fetch_all(&self.pool)
            .await?
        {
            let queries: Vec<String> =
                serde_json::from_str(&r.get::<String, _>("queries")).unwrap_or_default();
            if queries
                .iter()
                .any(|q| crate::store::links::normalize_query(q) == want)
            {
                out.push(r.get("id"));
            }
        }
        Ok(out)
    }

    /// A vector under another model, or an empty one, is left out. Merged newest first across
    /// kinds, because `gap_label_prompt` keeps only the first twelve.
    pub async fn open_gaps(&self, embed_model: &str, weak_below: f32) -> Result<OpenGaps> {
        let mut out: Vec<(i64, GapVec)> = Vec::new();
        // One row past the cap, so exactly `MAX_OPEN_GAPS` is not mistaken for truncation.
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(ask_gaps_sql!(", query_vec, judged_at"))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS + 1)
            .fetch_all(&self.pool)
            .await?
        {
            out.push((
                r.get("judged_at"),
                GapVec {
                    gap: Gap {
                        kind: GapKind::Ask,
                        id: r.get("id"),
                        text: r.get("text"),
                    },
                    vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                },
            ));
        }
        let asks_capped = out.len() as i64 > MAX_OPEN_GAPS;
        out.truncate(MAX_OPEN_GAPS as usize);
        let asks = out.len();
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(search_gaps_sql!(", query_vec, judged_at"))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS + 1)
            .fetch_all(&self.pool)
            .await?
        {
            out.push((
                r.get("judged_at"),
                GapVec {
                    gap: Gap {
                        kind: GapKind::Search,
                        id: r.get("id"),
                        text: r.get("text"),
                    },
                    vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                },
            ));
        }
        // Counted per kind, because each was capped on its own.
        let searches_capped = (out.len() - asks) as i64 > MAX_OPEN_GAPS;
        out.truncate(asks + MAX_OPEN_GAPS as usize);
        let searches = out.len() - asks;
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(unmatched_gaps_sql!(
            ", e.query_vec, e.created_at AS judged_at"
        ))
        .bind(embed_model)
        .bind(weak_below)
        .bind(MAX_OPEN_GAPS + 1)
        .fetch_all(&self.pool)
        .await?
        {
            out.push((
                r.get("judged_at"),
                GapVec {
                    gap: Gap {
                        kind: GapKind::Unmatched,
                        id: r.get("id"),
                        text: r.get("text"),
                    },
                    vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                },
            ));
        }
        let unmatched_capped = (out.len() - asks - searches) as i64 > MAX_OPEN_GAPS;
        out.truncate(asks + searches + MAX_OPEN_GAPS as usize);
        let unmatched = out.len() - asks - searches;
        let before_pursuits = out.len();
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(pursuit_gaps_sql!(", query_vec, opened_at"))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS + 1)
            .fetch_all(&self.pool)
            .await?
        {
            out.push((
                r.get("opened_at"),
                GapVec {
                    gap: Gap {
                        kind: GapKind::Pursuit,
                        id: r.get("id"),
                        text: pursuit_text(&r.get::<String, _>("queries")),
                    },
                    vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                },
            ));
        }
        let pursuits_capped = (out.len() - before_pursuits) as i64 > MAX_OPEN_GAPS;
        out.truncate(before_pursuits + MAX_OPEN_GAPS as usize);
        let pursuits = out.len() - before_pursuits;
        let before_subjects = out.len();
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(subject_gaps_sql!(", s.query_vec, s.created_at"))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS + 1)
            .fetch_all(&self.pool)
            .await?
        {
            out.push((
                r.get("created_at"),
                GapVec {
                    gap: Gap {
                        kind: GapKind::Subject,
                        id: r.get("id"),
                        text: r.get("text"),
                    },
                    vec: blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                },
            ));
        }
        let subjects_capped = (out.len() - before_subjects) as i64 > MAX_OPEN_GAPS;
        out.truncate(before_subjects + MAX_OPEN_GAPS as usize);
        let subjects = out.len() - before_subjects;
        let capped = asks_capped
            || searches_capped
            || unmatched_capped
            || pursuits_capped
            || subjects_capped;
        // Ties in whole-second `judged_at` broken by the uuid v7 id.
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.gap.id.cmp(&a.1.gap.id)));
        let out: Vec<GapVec> = out.into_iter().map(|(_, g)| g).collect();
        if capped {
            tracing::info!(
                cap = MAX_OPEN_GAPS,
                asks,
                searches,
                unmatched,
                pursuits,
                subjects,
                "more open gaps than one pass reads; the oldest are left out of this one"
            );
        }
        Ok(OpenGaps { gaps: out, capped })
    }

    /// Only uncovered subjects get a row. The vector comes from the fan-out's query cache, so
    /// no model call; an empty one is stored, since every reader gates on `vec_dim > 0`.
    pub async fn record_uncovered_subject(
        &self,
        event_id: &str,
        subject: &str,
        query_vec: &[f32],
        embed_model: &str,
    ) -> Result<String> {
        let id = crate::store::new_id();
        sqlx::query!(
            "INSERT INTO ask_subjects
               (id, event_id, subject, query_vec, vec_dim, embed_model, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &id,
            event_id,
            subject,
            crate::store::feedback::vec_to_blob(query_vec),
            query_vec.len() as i64,
            embed_model,
            now()
        )
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// The capture page opens the app; it must not decode every gap's vector.
    pub async fn open_gap_refs(&self, embed_model: &str, weak_below: f32) -> Result<Vec<Gap>> {
        let mut out = Vec::new();
        for (sql, kind) in [
            (ask_gaps_sql!(""), GapKind::Ask),
            (search_gaps_sql!(""), GapKind::Search),
            (subject_gaps_sql!(""), GapKind::Subject),
        ] {
            // runtime: the gap kinds share their predicates through macros.
            for r in sqlx::query(sql)
                .bind(embed_model)
                .bind(MAX_OPEN_GAPS)
                .fetch_all(&self.pool)
                .await?
            {
                out.push(Gap {
                    kind,
                    id: r.get("id"),
                    text: r.get("text"),
                });
            }
        }
        // One more bind than the other two, so it is read on its own.
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(unmatched_gaps_sql!(""))
            .bind(embed_model)
            .bind(weak_below)
            .bind(MAX_OPEN_GAPS)
            .fetch_all(&self.pool)
            .await?
        {
            out.push(Gap {
                kind: GapKind::Unmatched,
                id: r.get("id"),
                text: r.get("text"),
            });
        }
        // runtime: the gap kinds share their predicates through macros.
        for r in sqlx::query(pursuit_gaps_sql!(""))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS)
            .fetch_all(&self.pool)
            .await?
        {
            out.push(Gap {
                kind: GapKind::Pursuit,
                id: r.get("id"),
                text: pursuit_text(&r.get::<String, _>("queries")),
            });
        }
        Ok(out)
    }

    /// Not `state = 'unsatisfied'` alone: a pursuit a capture answered keeps its state but
    /// leaves the gap list. The same predicate as the list.
    pub async fn open_pursuit_gap_ids(
        &self,
        embed_model: &str,
    ) -> Result<std::collections::HashSet<String>> {
        // runtime: the gap kinds share their predicates through macros.
        Ok(sqlx::query(pursuit_gaps_sql!(""))
            .bind(embed_model)
            .bind(MAX_OPEN_GAPS)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("id"))
            .collect())
    }

    /// Uncapped: `open_gap_refs` saturates at five times `MAX_OPEN_GAPS`.
    pub async fn count_open_gaps(&self, embed_model: &str, weak_below: f32) -> Result<i64> {
        use sqlx::Row;
        let mut n: i64 = 0;
        for sql in [
            concat!("SELECT COUNT(*) AS n", ask_gaps_from!()),
            concat!("SELECT COUNT(*) AS n", search_gaps_from!()),
            concat!("SELECT COUNT(*) AS n", subject_gaps_from!()),
            concat!("SELECT COUNT(*) AS n", pursuit_gaps_from!()),
        ] {
            // runtime: the gap kinds share their predicates through macros.
            n += sqlx::query(sql)
                .bind(embed_model)
                .fetch_one(&self.pool)
                .await?
                .get::<i64, _>("n");
        }
        // One more bind than the other four, so it is read on its own.
        // runtime: the gap kinds share their predicates through macros.
        n += sqlx::query(concat!("SELECT COUNT(*) AS n", unmatched_gaps_from!()))
            .bind(embed_model)
            .bind(weak_below)
            .fetch_one(&self.pool)
            .await?
            .get::<i64, _>("n");
        Ok(n)
    }

    /// Uncapped, unlike `open_pursuit_gap_ids`.
    pub async fn count_open_pursuit_gaps(&self, embed_model: &str) -> Result<i64> {
        use sqlx::Row;
        Ok(
            // runtime: the gap kinds share their predicates through macros.
            sqlx::query(concat!("SELECT COUNT(*) AS n", pursuit_gaps_from!()))
                .bind(embed_model)
                .fetch_one(&self.pool)
                .await?
                .get("n"),
        )
    }

    pub async fn calibration_vecs(&self, embed_model: &str) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::new();
        for sql in [
            "SELECT query_vec FROM ask_events WHERE embed_model = ? AND vec_dim > 0
             ORDER BY id DESC LIMIT ?",
            "SELECT query_vec FROM search_events WHERE embed_model = ? AND vec_dim > 0
             ORDER BY id DESC LIMIT ?",
        ] {
            // runtime: the gap kinds share their predicates through macros.
            for r in sqlx::query(sql)
                .bind(embed_model)
                .bind(CALIBRATION_SAMPLE)
                .fetch_all(&self.pool)
                .await?
            {
                let v = blob_to_vec(&r.get::<Vec<u8>, _>("query_vec"));
                if !v.is_empty() {
                    out.push(v);
                }
            }
        }
        Ok(out)
    }

    /// Silent and reversible: the source row is untouched; deleting this row reopens the gap.
    pub async fn cover_gap(
        &self,
        kind: GapKind,
        gap_id: &str,
        corpus_id: &str,
        artifact_id: &str,
        score: f32,
        by: CoveredBy,
    ) -> Result<()> {
        sqlx::query!(
            "INSERT OR REPLACE INTO gap_coverage
               (kind, gap_id, corpus_id, artifact_id, score, covered_at, covered_by)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            kind.as_str(),
            gap_id,
            corpus_id,
            artifact_id,
            score,
            now(),
            by.as_str()
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn gaps_covered_by(&self, corpus_id: &str) -> Result<Vec<Gap>> {
        let one = [corpus_id.to_string()];
        Ok(self
            .gaps_covered_by_each(&one)
            .await?
            .remove(corpus_id)
            .unwrap_or_default())
    }

    /// One statement, not one per capture: the queue fragment is polled while work is in flight.
    /// Captures with nothing covered are absent from the map.
    pub async fn gaps_covered_by_each(
        &self,
        corpus_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Gap>>> {
        let mut out: std::collections::HashMap<String, Vec<Gap>> = std::collections::HashMap::new();
        if corpus_ids.is_empty() {
            return Ok(out);
        }
        // The ids are bound; only a count of placeholders is spliced.
        let holes = vec!["?"; corpus_ids.len()].join(", ");
        // runtime: one placeholder per item in the IN list.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT c.corpus_id, c.kind, c.gap_id,
                    COALESCE(a.question, s.query, p.queries, sub.subject) AS text
               FROM gap_coverage c
               LEFT JOIN ask_events a    ON c.kind = 'ask'    AND a.id = c.gap_id
               LEFT JOIN search_events s ON c.kind IN ('search', 'unmatched') AND s.id = c.gap_id
               LEFT JOIN pursuits p      ON c.kind = 'pursuit' AND p.id = c.gap_id
               LEFT JOIN ask_subjects sub ON c.kind = 'subject' AND sub.id = c.gap_id
              WHERE c.corpus_id IN ({holes})
              ORDER BY c.covered_at DESC"
        )));
        for id in corpus_ids {
            q = q.bind(id);
        }
        for r in q.fetch_all(&self.pool).await? {
            let Some(kind) = GapKind::parse(&r.get::<String, _>("kind")) else {
                continue;
            };
            let text: Option<String> = r.get("text");
            // The source can be gone until `trim_gap_coverage` collects the row; skipped meanwhile.
            let Some(text) = text else { continue };
            out.entry(r.get::<String, _>("corpus_id"))
                .or_default()
                .push(Gap {
                    kind,
                    id: r.get("gap_id"),
                    text: match kind {
                        GapKind::Pursuit => pursuit_text(&text),
                        _ => text,
                    },
                });
        }
        Ok(out)
    }

    /// `gap_id` names one of three tables, so no foreign key cascades. Also drops coverage
    /// scored under `below`, except `capture` rows, which no line wrote.
    pub async fn trim_gap_coverage(&self, below: f32) -> Result<u64> {
        Ok(sqlx::query!(
            "DELETE FROM gap_coverage
              WHERE (score < ? AND (covered_by IS NULL OR covered_by <> 'capture'))
                 OR (kind = 'ask'
                     AND NOT EXISTS (SELECT 1 FROM ask_events WHERE id = gap_coverage.gap_id))
                 OR (kind IN ('search', 'unmatched')
                     AND NOT EXISTS (SELECT 1 FROM search_events WHERE id = gap_coverage.gap_id))
                 OR (kind = 'pursuit'
                     AND NOT EXISTS (SELECT 1 FROM pursuits WHERE id = gap_coverage.gap_id))
                 OR (kind = 'subject'
                     AND NOT EXISTS (SELECT 1 FROM ask_subjects WHERE id = gap_coverage.gap_id))
                 OR kind NOT IN ('ask', 'search', 'unmatched', 'pursuit', 'subject')",
            below
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn uncover_gap(&self, kind: GapKind, gap_id: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM gap_coverage WHERE kind = ? AND gap_id = ?",
            kind.as_str(),
            gap_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn dismiss_gap(&self, kind: GapKind, id: &str) -> Result<()> {
        // Two literal statements: nothing from a request reaches the statement text.
        let res = match kind {
            GapKind::Ask => {
                sqlx::query!(
                    "UPDATE ask_events SET dismissed_at = ? WHERE id = ?",
                    now(),
                    id
                )
                .execute(&self.pool)
                .await?
            }
            // The same row as `Search`, dismissed for the same reason.
            GapKind::Search | GapKind::Unmatched => {
                sqlx::query!(
                    "UPDATE search_events SET dismissed_at = ? WHERE id = ?",
                    now(),
                    id
                )
                .execute(&self.pool)
                .await?
            }
            // `dismissed` is already a pursuit state, meaning the same thing.
            GapKind::Pursuit => {
                sqlx::query!(
                    "UPDATE pursuits SET state = 'dismissed', closed_at = ? WHERE id = ?",
                    now(),
                    id
                )
                .execute(&self.pool)
                .await?
            }
            // Its own column: a subject is a row of its own, not a reading of its question.
            GapKind::Subject => {
                sqlx::query!(
                    "UPDATE ask_subjects SET dismissed_at = ? WHERE id = ?",
                    now(),
                    id
                )
                .execute(&self.pool)
                .await?
            }
        };
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// The members let the sweep tell a replaced key from one it merely did not reach.
    pub async fn cluster_keys(&self) -> Result<Vec<StoredCluster>> {
        let mut out = Vec::new();
        for r in sqlx::query!(r#"SELECT key AS "key!", labelled_by, members FROM gap_clusters"#)
            .fetch_all(&self.pool)
            .await?
        {
            let raw: Vec<serde_json::Value> =
                serde_json::from_str(&r.members).map_err(|e| Error::Internal(e.to_string()))?;
            out.push(StoredCluster {
                key: r.key,
                labelled_by: r.labelled_by,
                members: raw
                    .iter()
                    .filter_map(|m| {
                        Some((
                            GapKind::parse(m["kind"].as_str()?)?,
                            m["id"].as_str()?.to_string(),
                        ))
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    pub async fn delete_clusters(&self, keys: &[String]) -> Result<()> {
        for k in keys {
            sqlx::query!("DELETE FROM gap_clusters WHERE key = ?", k)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn put_cluster(&self, c: &GapCluster) -> Result<()> {
        let members = serde_json::to_string(
            &c.members
                .iter()
                .map(|(k, id)| serde_json::json!({"kind": k.as_str(), "id": id}))
                .collect::<Vec<_>>(),
        )
        .map_err(|e| Error::Internal(e.to_string()))?;
        sqlx::query!(
            "INSERT INTO gap_clusters (key, label, labelled_by, members, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET label = excluded.label, labelled_by = excluded.labelled_by",
            &c.key,
            &c.label,
            &c.labelled_by,
            members,
            now()
        )
            .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Members dismissed since the sweep are absent; a row left with no members is not returned.
    pub async fn gap_rows(
        &self,
        embed_model: &str,
        weak_below: f32,
    ) -> Result<(Vec<GapRow>, Vec<Gap>)> {
        let open = self.open_gap_refs(embed_model, weak_below).await?;
        // Indexed once: a linear `find` per member was a million moves at the cap.
        let index: std::collections::HashMap<(GapKind, &str), &Gap> =
            open.iter().map(|g| ((g.kind, g.id.as_str()), g)).collect();
        let mut clustered: std::collections::HashSet<(GapKind, &str)> = Default::default();
        let mut rows = Vec::new();
        for r in sqlx::query!(
            "SELECT label, labelled_by, members FROM gap_clusters ORDER BY created_at DESC"
        )
        .fetch_all(&self.pool)
        .await?
        {
            let members: Vec<serde_json::Value> =
                serde_json::from_str(&r.members).map_err(|e| Error::Internal(e.to_string()))?;
            let mut resolved = Vec::new();
            for m in members {
                let kind = m["kind"].as_str().and_then(GapKind::parse);
                let id = m["id"].as_str();
                if let (Some(kind), Some(id)) = (kind, id)
                    && let Some(g) = index.get(&(kind, id))
                {
                    resolved.push((*g).clone());
                    clustered.insert((g.kind, g.id.as_str()));
                }
            }
            if !resolved.is_empty() {
                rows.push(GapRow {
                    label: r.label,
                    labelled_by: r.labelled_by,
                    members: resolved,
                });
            }
        }
        // Newest first, the order the query returned.
        let unclustered = open
            .iter()
            .filter(|g| !clustered.contains(&(g.kind, g.id.as_str())))
            .cloned()
            .collect();
        Ok((rows, unclustered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::asks::{AskVerdict, NewAsk};
    use crate::store::feedback::{Door, NewEvent, Verdict};

    async fn nothing_here(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_ask(NewAsk {
                question: q.into(),
                filters: "{}".into(),
                query_vec: vec,
                embed_model: "fake".into(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                ..Default::default()
            })
            .await
            .unwrap();
        store.judge_ask(&id, AskVerdict::NothingHere).await.unwrap();
        id
    }

    async fn uncovered_subject(store: &Store, q: &str, subject: &str, vec: Vec<f32>) -> String {
        let ask = store
            .record_ask(NewAsk {
                question: q.into(),
                filters: "{}".into(),
                query_vec: vec![1.0, 0.0],
                embed_model: "fake".into(),
                answer: "here is what I found".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .record_uncovered_subject(&ask, subject, &vec, "fake")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_uncovered_subject_is_an_open_gap() {
        let store = Store::memory().await.unwrap();
        uncovered_subject(&store, "how do ticks work", "job priority", vec![0.0, 1.0]).await;

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        let subjects: Vec<&str> = gaps
            .iter()
            .filter(|g| g.gap.kind == GapKind::Subject)
            .map(|g| g.gap.text.as_str())
            .collect();
        assert_eq!(subjects, vec!["job priority"]);
    }

    #[tokio::test]
    async fn a_covered_subject_leaves_the_list() {
        let store = Store::memory().await.unwrap();
        let id =
            uncovered_subject(&store, "how do ticks work", "job priority", vec![0.0, 1.0]).await;
        // Coverage carries foreign keys to a real capture.
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let made = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "job priority is a column".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .cover_gap(
                GapKind::Subject,
                &id,
                &src.id,
                &made[0].id,
                0.9,
                CoveredBy::Distance,
            )
            .await
            .unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        assert!(
            !gaps.iter().any(|g| g.gap.kind == GapKind::Subject),
            "a covered subject stayed on the list"
        );
    }

    #[tokio::test]
    async fn a_covered_subject_survives_the_repair_pass() {
        let store = Store::memory().await.unwrap();
        let id =
            uncovered_subject(&store, "how do ticks work", "job priority", vec![0.0, 1.0]).await;
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let made = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "job priority is a column".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .cover_gap(
                GapKind::Subject,
                &id,
                &src.id,
                &made[0].id,
                0.9,
                CoveredBy::Distance,
            )
            .await
            .unwrap();

        assert_eq!(
            store.trim_gap_coverage(0.0).await.unwrap(),
            0,
            "the repair pass collected a coverage whose subject still exists"
        );
        let covered = store
            .gaps_covered_by_each(std::slice::from_ref(&src.id))
            .await
            .unwrap();
        let named: Vec<&str> = covered
            .get(&src.id)
            .map(|v| v.iter().map(|c| c.text.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(named, vec!["job priority"]);

        sqlx::query!("DELETE FROM ask_subjects WHERE id = ?", &id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.trim_gap_coverage(0.0).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn coverage_under_the_line_is_collected_by_the_repair_pass() {
        let store = Store::memory().await.unwrap();
        let id =
            uncovered_subject(&store, "how do ticks work", "job priority", vec![0.0, 1.0]).await;
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let made = store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "job priority is a column".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .cover_gap(
                GapKind::Subject,
                &id,
                &src.id,
                &made[0].id,
                0.52,
                CoveredBy::Distance,
            )
            .await
            .unwrap();
        assert_eq!(store.trim_gap_coverage(0.5).await.unwrap(), 0);
        assert_eq!(store.trim_gap_coverage(0.6).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_subject_with_no_vector_is_not_a_gap() {
        let store = Store::memory().await.unwrap();
        uncovered_subject(&store, "how do ticks work", "job priority", vec![]).await;

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        assert!(!gaps.iter().any(|g| g.gap.kind == GapKind::Subject));
    }

    #[tokio::test]
    async fn a_subject_dies_with_the_question_it_came_from() {
        let store = Store::memory().await.unwrap();
        uncovered_subject(&store, "how do ticks work", "job priority", vec![0.0, 1.0]).await;
        sqlx::query!("DELETE FROM ask_events")
            .execute(&store.pool)
            .await
            .unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        assert!(!gaps.iter().any(|g| g.gap.kind == GapKind::Subject));
    }

    async fn gap_search(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door: Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        store
            .judge(&id, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        id
    }

    async fn search_with(store: &Store, q: &str, sims: &[f32]) -> String {
        store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: q.into(),
                    door: Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0],
                    embed_model: "fake".into(),
                    candidates: sims
                        .iter()
                        .enumerate()
                        .map(|(i, s)| crate::store::feedback::NewCandidate {
                            artifact_id: format!("a-{i}"),
                            score: *s,
                            similarity: Some(*s),
                            shown: true,
                            ..Default::default()
                        })
                        .collect(),
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_search_nothing_came_close_to_is_a_gap() {
        let store = Store::memory().await.unwrap();
        let far = search_with(&store, "mount an E01", &[0.20, 0.11]).await;
        search_with(&store, "grep a pcap", &[0.51, 0.10]).await;

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;

        let unmatched: Vec<&str> = gaps
            .iter()
            .filter(|g| g.gap.kind == GapKind::Unmatched)
            .map(|g| g.gap.id.as_str())
            .collect();
        assert_eq!(unmatched, vec![far.as_str()]);
    }

    #[tokio::test]
    async fn a_search_that_returned_nothing_is_an_unmatched_gap() {
        // NULL is not under the line, and the deck will not deal an empty list.
        let store = Store::memory().await.unwrap();
        let empty = search_with(&store, "mount an E01", &[]).await;

        let unmatched: Vec<String> = store
            .open_gaps("fake", 0.35)
            .await
            .unwrap()
            .gaps
            .iter()
            .filter(|g| g.gap.kind == GapKind::Unmatched)
            .map(|g| g.gap.id.clone())
            .collect();
        assert_eq!(unmatched, vec![empty]);
    }

    #[tokio::test]
    async fn a_query_too_short_for_the_deck_is_not_a_gap_either() {
        // `discard` is only reachable from the deck, which holds back anything under three chars.
        let store = Store::memory().await.unwrap();
        search_with(&store, "qq", &[]).await;
        search_with(&store, "qq", &[0.01]).await;

        let unmatched: Vec<String> = store
            .open_gaps("fake", 0.35)
            .await
            .unwrap()
            .gaps
            .iter()
            .filter(|g| g.gap.kind == GapKind::Unmatched)
            .map(|g| g.gap.id.clone())
            .collect();
        assert!(unmatched.is_empty(), "{unmatched:?}");
    }

    #[tokio::test]
    async fn a_search_whose_candidates_were_never_scored_is_not_a_gap() {
        // Unscored candidates: the embedder was down or keywords answered alone.
        let store = Store::memory().await.unwrap();
        store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: "mount an E01".into(),
                    door: Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![1.0, 0.0],
                    embed_model: "fake".into(),
                    candidates: vec![crate::store::feedback::NewCandidate {
                        artifact_id: "a-0".into(),
                        score: 0.9,
                        shown: true,
                        ..Default::default()
                    }],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();

        assert!(
            store
                .open_gaps("fake", 0.35)
                .await
                .unwrap()
                .gaps
                .iter()
                .all(|g| g.gap.kind != GapKind::Unmatched)
        );
    }

    #[tokio::test]
    async fn a_search_judged_a_gap_is_not_also_an_unmatched_one() {
        let store = Store::memory().await.unwrap();
        let id = search_with(&store, "mount an E01", &[0.10]).await;
        store
            .judge(&id, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        let mine: Vec<GapKind> = gaps
            .iter()
            .filter(|g| g.gap.id == id)
            .map(|g| g.gap.kind)
            .collect();
        assert_eq!(mine, vec![GapKind::Search]);
    }

    #[tokio::test]
    async fn a_search_the_operator_settled_is_not_an_unmatched_one() {
        // A typo scores below the line; a weak `hit` means poor ranking, not missing knowledge.
        let store = Store::memory().await.unwrap();
        let typo = search_with(&store, "mont an E01", &[0.10]).await;
        store
            .judge(
                &typo,
                Verdict::Discard,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();
        let weak_hit = search_with(&store, "grep a pcap", &[0.12]).await;
        store
            .judge(
                &weak_hit,
                Verdict::Hit,
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;

        assert!(
            gaps.iter().all(|g| g.gap.kind != GapKind::Unmatched),
            "a judged search is settled, whatever the judgement was: {:?}",
            gaps.iter().map(|g| &g.gap.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn the_gaps_a_page_of_captures_answered_come_back_grouped() {
        let store = Store::memory().await.unwrap();
        // `gap_coverage` has foreign keys to both the capture and the artifact.
        for (c, a) in [("corpus-a", "art-1"), ("corpus-b", "art-2")] {
            sqlx::query!(
                "INSERT INTO corpora (id, raw_text, origin, content_hash, status,
                                      created_at, updated_at)
                 VALUES (?, 'text', 'paste', ?, 'ready', 0, 0)",
                c,
                c
            )
            .execute(&store.pool)
            .await
            .unwrap();
            sqlx::query!(
                "INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
                 VALUES (?, ?, 0, 'text', 0)",
                a,
                c
            )
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let one = search_with(&store, "mount an E01", &[0.10]).await;
        let two = search_with(&store, "grep a pcap", &[0.10]).await;
        store
            .cover_gap(
                GapKind::Unmatched,
                &one,
                "corpus-a",
                "art-1",
                0.8,
                CoveredBy::Distance,
            )
            .await
            .unwrap();
        store
            .cover_gap(
                GapKind::Unmatched,
                &two,
                "corpus-b",
                "art-2",
                0.9,
                CoveredBy::Distance,
            )
            .await
            .unwrap();

        let by_corpus = store
            .gaps_covered_by_each(&[
                "corpus-a".to_string(),
                "corpus-b".to_string(),
                "corpus-c".to_string(),
            ])
            .await
            .unwrap();

        assert_eq!(
            by_corpus["corpus-a"]
                .iter()
                .map(|g| g.text.as_str())
                .collect::<Vec<_>>(),
            vec!["mount an E01"]
        );
        assert_eq!(
            by_corpus["corpus-b"]
                .iter()
                .map(|g| g.text.as_str())
                .collect::<Vec<_>>(),
            vec!["grep a pcap"]
        );
        assert!(
            !by_corpus.contains_key("corpus-c"),
            "a capture that answered nothing is absent, not present and empty"
        );
        assert_eq!(
            store
                .gaps_covered_by("corpus-a")
                .await
                .unwrap()
                .into_iter()
                .map(|g| g.text)
                .collect::<Vec<_>>(),
            vec!["mount an E01".to_string()]
        );
    }

    #[tokio::test]
    async fn a_covered_search_stays_covered_when_it_is_later_judged_a_gap() {
        // One row, two readings: a search already answered as `unmatched` stays answered as a gap.
        let store = Store::memory().await.unwrap();
        sqlx::query!(
            "INSERT INTO corpora (id, raw_text, origin, content_hash, status,
                                  created_at, updated_at)
             VALUES ('corpus-a', 'text', 'paste', 'h', 'ready', 0, 0)"
        )
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
             VALUES ('art-1', 'corpus-a', 0, 'text', 0)"
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let id = search_with(&store, "mount an E01", &[0.10]).await;
        store
            .cover_gap(
                GapKind::Unmatched,
                &id,
                "corpus-a",
                "art-1",
                0.8,
                CoveredBy::Distance,
            )
            .await
            .unwrap();
        assert!(store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());

        store
            .judge(&id, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();

        assert!(
            store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty(),
            "a capture already answered this search; judging it reopened it"
        );
        store.uncover_gap(GapKind::Unmatched, &id).await.unwrap();
        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].gap.kind, GapKind::Search);
    }

    #[tokio::test]
    async fn coverage_of_a_gap_that_is_gone_is_collected() {
        // `gap_id` cannot be a foreign key, so nothing else deletes the coverage row.
        let store = Store::memory().await.unwrap();
        sqlx::query!(
            "INSERT INTO corpora (id, raw_text, origin, content_hash, status,
                                  created_at, updated_at)
             VALUES ('corpus-a', 'text', 'paste', 'h', 'ready', 0, 0)"
        )
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
             VALUES ('art-1', 'corpus-a', 0, 'text', 0)"
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let kept = search_with(&store, "mount an E01", &[0.10]).await;
        let expired = search_with(&store, "grep a pcap", &[0.10]).await;
        for id in [&kept, &expired] {
            store
                .cover_gap(
                    GapKind::Unmatched,
                    id,
                    "corpus-a",
                    "art-1",
                    0.8,
                    CoveredBy::Distance,
                )
                .await
                .unwrap();
        }
        sqlx::query!("DELETE FROM search_events WHERE id = ?", &expired)
            .execute(&store.pool)
            .await
            .unwrap();

        assert_eq!(store.trim_gap_coverage(0.0).await.unwrap(), 1);

        let left: Vec<String> = sqlx::query!("SELECT gap_id FROM gap_coverage")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.gap_id)
            .collect();
        assert_eq!(left, vec![kept], "the coverage of a gap that still exists");
        assert_eq!(store.trim_gap_coverage(0.0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dismissing_an_unmatched_search_closes_it() {
        let store = Store::memory().await.unwrap();
        let id = search_with(&store, "mount an E01", &[0.10]).await;
        assert!(!store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());

        store.dismiss_gap(GapKind::Unmatched, &id).await.unwrap();

        assert!(store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());
    }

    #[tokio::test]
    async fn a_pursuit_that_ended_unsatisfied_is_a_gap() {
        let store = Store::memory().await.unwrap();
        let id = store
            .insert_pursuit(
                100,
                &["how do I mount an E01".into(), "E01 mount".into()],
                &[],
                Some((&[1.0, 0.0], "fake")),
            )
            .await
            .unwrap();
        store
            .close_pursuit(&id, "unsatisfied", "nothing strong was engaged", 200)
            .await
            .unwrap();
        let happy = store
            .insert_pursuit(
                300,
                &["grep a pcap".into()],
                &[],
                Some((&[0.0, 1.0], "fake")),
            )
            .await
            .unwrap();
        store
            .close_pursuit(&happy, "satisfied", "a strong hit was engaged", 400)
            .await
            .unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        let mine: Vec<(&str, &str)> = gaps
            .iter()
            .filter(|g| g.gap.kind == GapKind::Pursuit)
            .map(|g| (g.gap.id.as_str(), g.gap.text.as_str()))
            .collect();
        assert_eq!(mine, vec![(id.as_str(), "how do I mount an E01")]);
    }

    #[tokio::test]
    async fn the_pursuit_gap_count_is_a_total_and_not_the_length_of_a_page() {
        let store = Store::memory().await.unwrap();
        for i in 0..60i64 {
            let id = store
                .insert_pursuit(
                    i * 10,
                    &[format!("how do I mount an E01 {i}")],
                    &[],
                    Some((&[1.0, 0.0], "fake")),
                )
                .await
                .unwrap();
            store
                .close_pursuit(&id, "unsatisfied", "nothing strong was engaged", i * 10 + 5)
                .await
                .unwrap();
        }
        // Sixty, not the fifty a page of pursuits would have held.
        assert_eq!(store.count_open_pursuit_gaps("fake").await.unwrap(), 60);
        assert_eq!(store.count_pursuits("unsatisfied").await.unwrap(), 60);
        assert_eq!(store.count_pursuits("open").await.unwrap(), 0);
        assert_eq!(
            store.open_pursuit_gap_ids("fake").await.unwrap().len() as i64,
            store.count_open_pursuit_gaps("fake").await.unwrap()
        );
        store
            .dismiss_gap(
                GapKind::Pursuit,
                store
                    .open_pursuit_gap_ids("fake")
                    .await
                    .unwrap()
                    .iter()
                    .next()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(store.count_open_pursuit_gaps("fake").await.unwrap(), 59);
    }

    #[tokio::test]
    async fn a_pursuit_written_before_the_vector_was_carried_is_not_a_gap() {
        // An uncomparable vector is left out rather than compared anyway.
        let store = Store::memory().await.unwrap();
        let id = store
            .insert_pursuit(100, &["how do I mount an E01".into()], &[], None)
            .await
            .unwrap();
        store
            .close_pursuit(&id, "unsatisfied", "nothing strong was engaged", 200)
            .await
            .unwrap();

        assert!(store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());
    }

    #[tokio::test]
    async fn dismissing_a_pursuit_gap_closes_the_pursuit() {
        let store = Store::memory().await.unwrap();
        let id = store
            .insert_pursuit(100, &["q".into()], &[], Some((&[1.0, 0.0], "fake")))
            .await
            .unwrap();
        store
            .close_pursuit(&id, "unsatisfied", "nothing strong was engaged", 200)
            .await
            .unwrap();

        store.dismiss_gap(GapKind::Pursuit, &id).await.unwrap();

        assert_eq!(store.get_pursuit(&id).await.unwrap().state, "dismissed");
        assert!(store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());
    }

    #[tokio::test]
    async fn open_gaps_are_the_unanswered_questions_and_the_gap_searches_under_this_model() {
        let store = Store::memory().await.unwrap();
        nothing_here(&store, "q1", vec![1.0, 0.0]).await;
        gap_search(&store, "s1", vec![0.0, 1.0]).await;
        let right = store
            .record_ask(NewAsk {
                question: "ok".into(),
                filters: "{}".into(),
                query_vec: vec![1.0, 1.0],
                embed_model: "fake".into(),
                answer: "yes".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store.judge_ask(&right, AskVerdict::Right).await.unwrap();
        nothing_here(&store, "no vector", vec![]).await;
        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        // Same second, so the uuid v7 ids break the tie in recording order.
        assert_eq!(
            gaps.iter().map(|g| g.gap.text.as_str()).collect::<Vec<_>>(),
            vec!["s1", "q1"]
        );
        assert!(
            gaps.iter().all(|g| !g.vec.is_empty()),
            "the sweep's reader is the one that must carry the vectors"
        );
        assert!(
            store
                .open_gaps("other-model", 0.35)
                .await
                .unwrap()
                .gaps
                .is_empty()
        );

        let refs = store.open_gap_refs("fake", 0.35).await.unwrap();
        assert_eq!(
            refs.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            vec!["q1", "s1"]
        );
    }

    #[tokio::test]
    async fn one_pass_reads_at_most_the_newest_cap_worth_of_gaps() {
        let store = Store::memory().await.unwrap();
        for i in 0..MAX_OPEN_GAPS + 1 {
            nothing_here(&store, &format!("q{i}"), vec![1.0, 0.0]).await;
        }
        let open = store.open_gaps("fake", 0.35).await.unwrap();
        assert_eq!(open.gaps.len() as i64, MAX_OPEN_GAPS);
        assert_eq!(
            open.gaps[0].gap.text,
            format!("q{MAX_OPEN_GAPS}"),
            "the newest gap is the one a bounded pass must not drop"
        );
        assert!(
            open.capped,
            "a pass that left gaps out has to say so: the sweep reads a partial \
             pass as a set of dismissals otherwise"
        );
        assert_eq!(
            store.open_gap_refs("fake", 0.35).await.unwrap().len() as i64,
            MAX_OPEN_GAPS,
            "the display path is bounded by the same cap"
        );
        assert_eq!(
            store.count_open_gaps("fake", 0.35).await.unwrap(),
            MAX_OPEN_GAPS + 1,
            "a total is not a page: `--status` is opened to find out whether \
             the number is moving, and the length of a capped list stops"
        );
    }

    #[tokio::test]
    async fn the_count_of_open_gaps_is_the_length_of_the_list_it_is_under_the_cap() {
        let store = Store::memory().await.unwrap();
        nothing_here(&store, "q1", vec![1.0]).await;
        let s = gap_search(&store, "s1", vec![1.0]).await;
        assert_eq!(
            store.count_open_gaps("fake", 0.35).await.unwrap(),
            store.open_gap_refs("fake", 0.35).await.unwrap().len() as i64
        );
        assert_eq!(store.count_open_gaps("fake", 0.35).await.unwrap(), 2);
        store.dismiss_gap(GapKind::Search, &s).await.unwrap();
        assert_eq!(store.count_open_gaps("fake", 0.35).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_table_holding_exactly_the_cap_has_left_nothing_out() {
        // At exactly the cap a full page must not read as truncated.
        let store = Store::memory().await.unwrap();
        for i in 0..MAX_OPEN_GAPS {
            nothing_here(&store, &format!("q{i}"), vec![1.0, 0.0]).await;
        }
        let open = store.open_gaps("fake", 0.35).await.unwrap();
        assert_eq!(open.gaps.len() as i64, MAX_OPEN_GAPS);
        assert!(!open.capped, "nothing was left out of this pass");
    }

    #[tokio::test]
    async fn a_dismissed_gap_is_no_longer_open() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let s = gap_search(&store, "s1", vec![1.0]).await;
        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(store.open_gaps("fake", 0.35).await.unwrap().gaps.len(), 1);
        store.dismiss_gap(GapKind::Search, &s).await.unwrap();
        assert!(store.open_gaps("fake", 0.35).await.unwrap().gaps.is_empty());
        assert!(matches!(
            store.dismiss_gap(GapKind::Ask, "nope").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn rows_resolve_members_and_report_what_no_cluster_names_yet() {
        let store = Store::memory().await.unwrap();
        let a = nothing_here(&store, "q1", vec![1.0]).await;
        let b = nothing_here(&store, "q2", vec![1.0]).await;
        let later = nothing_here(&store, "q3", vec![1.0]).await;
        store
            .put_cluster(&GapCluster {
                key: "k".into(),
                label: "Mounting".into(),
                labelled_by: "model".into(),
                members: vec![(GapKind::Ask, a.clone()), (GapKind::Ask, b.clone())],
            })
            .await
            .unwrap();
        let (rows, loose) = store.gap_rows("fake", 0.35).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Mounting");
        assert_eq!(rows[0].members.len(), 2);
        assert_eq!(
            loose.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            vec![later.as_str()]
        );

        store.dismiss_gap(GapKind::Ask, &a).await.unwrap();
        assert_eq!(
            store.gap_rows("fake", 0.35).await.unwrap().0[0]
                .members
                .len(),
            1
        );
        store.dismiss_gap(GapKind::Ask, &b).await.unwrap();
        assert!(store.gap_rows("fake", 0.35).await.unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn clusters_can_be_listed_replaced_and_deleted() {
        let store = Store::memory().await.unwrap();
        let c = GapCluster {
            key: "k".into(),
            label: "x".into(),
            labelled_by: "terms".into(),
            members: vec![],
        };
        store.put_cluster(&c).await.unwrap();
        store
            .put_cluster(&GapCluster {
                label: "y".into(),
                labelled_by: "model".into(),
                ..c.clone()
            })
            .await
            .unwrap();
        let keys = store.cluster_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "k");
        assert_eq!(keys[0].labelled_by, "model");
        assert_eq!(keys[0].members, c.members);
        store.delete_clusters(&["k".into()]).await.unwrap();
        assert!(store.cluster_keys().await.unwrap().is_empty());
    }
}
