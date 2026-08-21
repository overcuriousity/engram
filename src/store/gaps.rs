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
    /// A recorded search where nothing came close: every candidate's similarity
    /// under `vector.weak_below`, which is what the rail was already saying at
    /// the time in as many words.
    ///
    /// Distance rather than behaviour. The first draft of this counted a search
    /// after which nothing was opened, and that was wrong twice over: not
    /// clicking a result can mean the list was useless or that the titles alone
    /// told the operator what they needed, and the two readings are opposite;
    /// and an open is only recorded when pursuits *and* feedback are on, so on
    /// most installs every search in the log looks abandoned. A distance needs
    /// no interaction data, works whatever else is switched on, and can be
    /// computed over the existing log retroactively. It is also the more honest
    /// claim: not *you gave up*, but *the base held nothing near this*.
    Unmatched,
    /// A pursuit that closed `unsatisfied`: a run of searches on one subject
    /// that the base did not answer. It landed on Ops and nowhere else, and its
    /// clustered queries were already stored — what it lacked was a vector, so
    /// the sweep now carries the leading query's forward when it writes the
    /// row. A pursuit written before that has none and is not a gap, which is
    /// the same rule `vec_dim > 0` already applies to everything else here.
    Pursuit,
}

impl GapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GapKind::Ask => "ask",
            GapKind::Search => "search",
            GapKind::Unmatched => "unmatched",
            GapKind::Pursuit => "pursuit",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(GapKind::Ask),
            "search" => Some(GapKind::Search),
            "unmatched" => Some(GapKind::Unmatched),
            "pursuit" => Some(GapKind::Pursuit),
            _ => None,
        }
    }
}

/// One open gap: a question the base could not answer, or a search judged to
/// have no answer.
#[derive(Debug, Clone)]
pub struct Gap {
    pub kind: GapKind,
    pub id: String,
    pub text: String,
}

/// A gap and the query vector it was found by.
///
/// Only the sweep needs the vector, and it is by far the expensive half of the
/// row: at bge-m3's 1024 dimensions a full pass is four million floats to read
/// out of SQLite and decode. The display path reads `Gap` alone — see
/// `open_gap_refs` — because nothing on the capture page looks at a vector.
#[derive(Debug, Clone)]
pub struct GapVec {
    pub gap: Gap,
    pub vec: Vec<f32>,
}

/// What one bounded pass read, and whether the bound bit.
///
/// `capped` is not only for the log. `jobs::gaps::sweep` deletes every cluster
/// whose key it did not see as stale, and a cluster whose members the cap left
/// out was never seen — deleting it took those gaps off the capture page
/// altogether rather than merely leaving them ungrouped.
pub struct OpenGaps {
    pub gaps: Vec<GapVec>,
    pub capped: bool,
}

#[derive(Debug, Clone)]
pub struct GapCluster {
    pub key: String,
    pub label: String,
    /// `model` or `terms`.
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

/// A stored cluster as `cluster_keys` reads it back: what it is keyed on and
/// how it was named, without the label the page renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCluster {
    pub key: String,
    /// `model` or `terms`.
    pub labelled_by: String,
    pub members: Vec<(GapKind, String)>,
}

/// A cluster as the capture page shows it.
#[derive(Debug, Clone)]
pub struct GapRow {
    pub label: String,
    pub labelled_by: String,
    pub members: Vec<Gap>,
}

/// How many open gaps one pass reads, per kind.
///
/// A cap rather than the whole table, because both readers scale badly in this
/// number and neither says so: `jobs::gaps::sweep` compares every pair of them
/// on every retention tick, and `ui::capture_page` — the page the app opens on —
/// walks the same list with its full query vectors on every load.
///
/// Per kind, and there are four of them, so what either reader actually gets is
/// up to four times this. The sweep's clustering is quadratic in that total and
/// the capture page renders one row of it apiece, which is two million cosines
/// on a timer and two thousand `<li>` before the first sweep has grouped
/// anything. Both are the accepted cost of showing every kind of gap rather
/// than the newest few hundred whatever kind they are — see `core::gaps::cluster`.
/// What is *not* accepted at that width is `jobs::gaps::cover`, which turns
/// each of them into a network round trip on a path a capture waits on; it
/// takes the newest `COVER_MAX_GAPS` and leaves the rest. `cluster`'s
/// "N is tens, so the quadratic pass is fine" was an assumption about an
/// operator's habits, not a property of the query; a few thousand searches
/// judged `gap` made both costs real.
///
/// Newest first, so what is dropped is the oldest — a gap judged this week is
/// the one someone is still trying to fill. `judged_at` is whole seconds, which
/// on its own leaves everything judged inside one second in whatever order the
/// table hands back; the id breaks the tie, and being uuid v7 it breaks it by
/// creation, so the cap never cuts across a single second arbitrarily. `open_gaps` logs when the cap bites,
/// because a grouping that quietly left half the gaps out would read on the page
/// exactly like a grouping of all of them.
pub const MAX_OPEN_GAPS: i64 = 500;

/// The predicate both readers of the open gaps share, once.
///
/// Two projections over one `WHERE`: the sweep needs `query_vec`, the capture
/// page needs only the words. Written as a macro rather than as four statements
/// because the two projections drifting apart would mean the page and the sweep
/// disagreeing about what is open — and because nothing from a request reaches
/// the statement text either way, the `embed_model` being bound.
macro_rules! ask_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, question AS text",
            $cols,
            " FROM ask_events
              WHERE verdict = 'nothing_here' AND dismissed_at IS NULL
                AND embed_model = ? AND vec_dim > 0
                AND NOT EXISTS (SELECT 1 FROM gap_coverage
                                 WHERE kind = 'ask' AND gap_id = ask_events.id)
              ORDER BY judged_at DESC, id DESC LIMIT ?"
        )
    };
}

macro_rules! search_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, query AS text",
            $cols,
            " FROM search_events
              WHERE verdict = 'gap' AND dismissed_at IS NULL
                AND embed_model = ? AND vec_dim > 0
                AND NOT EXISTS (SELECT 1 FROM gap_coverage
                                 WHERE kind = 'search' AND gap_id = search_events.id)
              ORDER BY judged_at DESC, id DESC LIMIT ?"
        )
    };
}

/// A search nothing came close to answering.
///
/// The similarity is not a column on `search_events` — it is on the candidate
/// rows, one per result — so the test is an aggregate rather than a read.
/// `MAX(...)` over no rows is `NULL`, and `NULL < ?` is not true, so a search
/// that recorded no candidates is *not* a gap. That looks like an oversight and
/// is not one: "nothing came close" is a claim about what was measured, and
/// nothing was.
///
/// A search that has been judged at all is left out. `gap` for the obvious
/// reason — it would be two gaps, the same row said twice in two different
/// words — but `discard` and `hit` equally, and those are the ones worth
/// naming. `discard` is the operator saying this was never a search: a typo, or
/// poking at the box. A typo's candidates are exactly what scores below
/// `weak_below`, so a rule that only skipped `gap` would take every dismissed
/// typo and put it back on the capture page as a hole in the base. And a `hit`
/// whose candidates all scored weakly is a search someone answered; the
/// scores say the ranking was poor, not that the knowledge is missing.
///
/// The guard this needs already exists. A typing burst folds into one event by
/// `feedback.coalesce_secs`, so what is measured is the finished query and not
/// its first two letters.
macro_rules! unmatched_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT e.id, e.query AS text",
            $cols,
            " FROM search_events e
              WHERE e.dismissed_at IS NULL AND e.embed_model = ? AND e.vec_dim > 0
                AND e.verdict IS NULL
                AND NOT EXISTS (SELECT 1 FROM gap_coverage
                                 WHERE kind = 'unmatched' AND gap_id = e.id)
                AND (SELECT MAX(c.similarity) FROM search_candidates c
                      WHERE c.event_id = e.id AND c.similarity IS NOT NULL) < ?
              ORDER BY e.created_at DESC, e.id DESC LIMIT ?"
        )
    };
}

/// A run of searches the base did not answer.
///
/// The text is the leading clustered query rather than all of them joined: the
/// naming prompt keeps the first twelve members, and a member that is itself a
/// paragraph of queries would crowd out eleven other gaps. `queries` is JSON,
/// so the first element is read out in Rust rather than in SQL.
macro_rules! pursuit_gaps_sql {
    ($cols:literal) => {
        concat!(
            "SELECT id, queries",
            $cols,
            " FROM pursuits
              WHERE state = 'unsatisfied' AND embed_model = ? AND vec_dim > 0
                AND NOT EXISTS (SELECT 1 FROM gap_coverage
                                 WHERE kind = 'pursuit' AND gap_id = pursuits.id)
              ORDER BY opened_at DESC, id DESC LIMIT ?"
        )
    };
}

/// The words a pursuit gap shows: its leading clustered query.
fn pursuit_text(queries_json: &str) -> String {
    serde_json::from_str::<Vec<String>>(queries_json)
        .ok()
        .and_then(|q| q.into_iter().next())
        .unwrap_or_default()
}

/// How many stored query vectors the linkage calibration reads, per table.
///
/// Every recorded search and question counts, not only the gaps: what
/// `core::gaps::link_threshold` measures is what *unrelated* queries score
/// under this embedder, and a sample drawn from one topic's worth of holes is
/// the one sample that cannot say. Two hundred a side is 79,800 pairs, well
/// under what the sweep's own clustering already costs.
pub const CALIBRATION_SAMPLE: i64 = 200;

impl Store {
    /// Every open gap with a vector under `embed_model`, newest first, up to
    /// `MAX_OPEN_GAPS` of each kind. A vector under another model is not
    /// comparable and is left out; an empty one (the cache had evicted it)
    /// likewise.
    ///
    /// Newest first across both kinds, not within each. The cap is per kind, so
    /// the two are read separately, but appending one list after the other left
    /// a cluster of mixed kinds ordered ask-then-search — and `sweep` hands that
    /// order to `gap_label_prompt`, which keeps the first twelve. A group with a
    /// dozen questions in it named itself from those and never saw a search gap,
    /// however recent.
    pub async fn open_gaps(&self, embed_model: &str, weak_below: f32) -> Result<OpenGaps> {
        // `(judged_at, GapVec)`: the sort key is not part of what the caller
        // gets, only of the order it gets it in.
        let mut out: Vec<(i64, GapVec)> = Vec::new();
        // One row past the cap, and truncated below. Reading exactly the cap and
        // calling that capped cannot tell a table with precisely `MAX_OPEN_GAPS`
        // open gaps — nothing left out — from one that was truncated, and now
        // that `sweep` changes what it does when capped, that false positive
        // would leave a base at exactly the cap never cleaning up a group again.
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
        let capped = asks_capped || searches_capped || unmatched_capped || pursuits_capped;
        // The same key each half was already read by, applied across both:
        // whole-second `judged_at`, ties broken by a uuid v7 id, so a second's
        // worth of gaps is still ordered by when they were recorded.
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.gap.id.cmp(&a.1.gap.id)));
        let out: Vec<GapVec> = out.into_iter().map(|(_, g)| g).collect();
        if capped {
            tracing::info!(
                cap = MAX_OPEN_GAPS,
                asks,
                searches,
                unmatched,
                pursuits,
                "more open gaps than one pass reads; the oldest are left out of this one"
            );
        }
        Ok(OpenGaps { gaps: out, capped })
    }

    /// The same open gaps, without their vectors: what the capture page renders.
    ///
    /// The page shows a gap's words, its kind and its id and nothing else, and
    /// it is the page the app opens on. Reading the vectors for it decoded four
    /// million floats per load to throw all of them away.
    pub async fn open_gap_refs(&self, embed_model: &str, weak_below: f32) -> Result<Vec<Gap>> {
        let mut out = Vec::new();
        for (sql, kind) in [
            (ask_gaps_sql!(""), GapKind::Ask),
            (search_gaps_sql!(""), GapKind::Search),
        ] {
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
        // One more bind than the other two, so it is read on its own rather
        // than joining the loop above.
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

    /// Query vectors from every recorded search and question under this
    /// embedder — gap or not, judged or not — newest first, `CALIBRATION_SAMPLE`
    /// of each. What `core::gaps::link_threshold` measures the embedder's own
    /// geometry from.
    pub async fn calibration_vecs(&self, embed_model: &str) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::new();
        for sql in [
            "SELECT query_vec FROM ask_events WHERE embed_model = ? AND vec_dim > 0
             ORDER BY id DESC LIMIT ?",
            "SELECT query_vec FROM search_events WHERE embed_model = ? AND vec_dim > 0
             ORDER BY id DESC LIMIT ?",
        ] {
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

    /// This capture answered this gap.
    ///
    /// Silent and reversible. The source row is untouched, so an operator who
    /// disagrees reopens the gap by deleting this row — and nothing is deleted
    /// on a score. Silent because a base with forty gaps would otherwise turn
    /// its own housekeeping into a review queue.
    pub async fn cover_gap(
        &self,
        kind: GapKind,
        gap_id: &str,
        corpus_id: &str,
        artifact_id: &str,
        score: f32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO gap_coverage
               (kind, gap_id, corpus_id, artifact_id, score, covered_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(kind.as_str())
        .bind(gap_id)
        .bind(corpus_id)
        .bind(artifact_id)
        .bind(score)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The gaps this capture answered, newest first: what the capture page says
    /// it did beyond being stored.
    pub async fn gaps_covered_by(&self, corpus_id: &str) -> Result<Vec<Gap>> {
        let one = [corpus_id.to_string()];
        Ok(self
            .gaps_covered_by_each(&one)
            .await?
            .remove(corpus_id)
            .unwrap_or_default())
    }

    /// The same, for a list of captures at once.
    ///
    /// One statement rather than one per capture. The queue fragment renders a
    /// page of rows and the capture page polls it while anything is in flight,
    /// so asking per row makes a three-way `LEFT JOIN` into a round of them on
    /// a hot path — for an answer the same join gives in a single pass.
    ///
    /// Captures with nothing covered are absent from the map rather than
    /// present and empty, which is the same thing to every caller: they ask for
    /// a list and take `unwrap_or_default`.
    pub async fn gaps_covered_by_each(
        &self,
        corpus_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Gap>>> {
        let mut out: std::collections::HashMap<String, Vec<Gap>> = std::collections::HashMap::new();
        if corpus_ids.is_empty() {
            return Ok(out);
        }
        // The ids are bound, never spliced; what is spliced is a count of
        // placeholders.
        let holes = vec!["?"; corpus_ids.len()].join(", ");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT c.corpus_id, c.kind, c.gap_id,
                    COALESCE(a.question, s.query, p.queries) AS text
               FROM gap_coverage c
               LEFT JOIN ask_events a    ON c.kind = 'ask'    AND a.id = c.gap_id
               LEFT JOIN search_events s ON c.kind IN ('search', 'unmatched') AND s.id = c.gap_id
               LEFT JOIN pursuits p      ON c.kind = 'pursuit' AND p.id = c.gap_id
              WHERE c.corpus_id IN ({holes})
              ORDER BY c.covered_at DESC"
        )));
        for id in corpus_ids {
            q = q.bind(id);
        }
        // One descending order over the whole set is still newest-first inside
        // each capture, because a filter never reorders what it keeps.
        for r in q.fetch_all(&self.pool).await? {
            let Some(kind) = GapKind::parse(&r.get::<String, _>("kind")) else {
                continue;
            };
            let text: Option<String> = r.get("text");
            // The source row can be gone — a search expired by retention, a
            // pursuit purged. The coverage row outlives it and says nothing
            // useful without it, so it is skipped rather than rendered blank.
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

    /// Undo a coverage: the gap is open again, and the judgement behind it was
    /// never touched.
    pub async fn uncover_gap(&self, kind: GapKind, gap_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM gap_coverage WHERE kind = ? AND gap_id = ?")
            .bind(kind.as_str())
            .bind(gap_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn dismiss_gap(&self, kind: GapKind, id: &str) -> Result<()> {
        // Two literal statements rather than one built from the kind: nothing
        // from a request reaches the statement text.
        let res = match kind {
            GapKind::Ask => {
                sqlx::query("UPDATE ask_events SET dismissed_at = ? WHERE id = ?")
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
            // The same column `Search` writes, and correctly so: it is the same
            // row, dismissed for the same reason. A search dismissed as
            // unmatched is not offered again if it is later judged a gap
            // either — the operator has already said this one is answered.
            GapKind::Search | GapKind::Unmatched => {
                sqlx::query("UPDATE search_events SET dismissed_at = ? WHERE id = ?")
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
            // A pursuit has a state rather than a dismissal column, and
            // `dismissed` is already one of the states it can be in — set by
            // the operator on Ops, meaning the same thing it means here.
            GapKind::Pursuit => {
                sqlx::query("UPDATE pursuits SET state = 'dismissed', closed_at = ? WHERE id = ?")
                    .bind(now())
                    .bind(id)
                    .execute(&self.pool)
                    .await?
            }
        };
        if res.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Every stored cluster, with the members it was keyed on.
    ///
    /// The members are what lets `jobs::gaps::sweep` tell a key this pass has
    /// replaced from one it merely did not reach, which is the difference
    /// between removing a stale heading and taking a live gap off the page.
    pub async fn cluster_keys(&self) -> Result<Vec<StoredCluster>> {
        let mut out = Vec::new();
        for r in sqlx::query("SELECT key, labelled_by, members FROM gap_clusters")
            .fetch_all(&self.pool)
            .await?
        {
            let raw: Vec<serde_json::Value> = serde_json::from_str(&r.get::<String, _>("members"))
                .map_err(|e| Error::Internal(e.to_string()))?;
            out.push(StoredCluster {
                key: r.get("key"),
                labelled_by: r.get("labelled_by"),
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
            sqlx::query("DELETE FROM gap_clusters WHERE key = ?")
                .bind(k)
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
        sqlx::query(
            "INSERT INTO gap_clusters (key, label, labelled_by, members, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET label = excluded.label, labelled_by = excluded.labelled_by",
        )
        .bind(&c.key)
        .bind(&c.label)
        .bind(&c.labelled_by)
        .bind(members)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The clusters with their open members resolved, and the open gaps no
    /// cluster names yet (judged since the last sweep). A member that has been
    /// dismissed since the sweep is simply absent from its row; a row left with
    /// no members is not returned.
    pub async fn gap_rows(
        &self,
        embed_model: &str,
        weak_below: f32,
    ) -> Result<(Vec<GapRow>, Vec<Gap>)> {
        let open = self.open_gap_refs(embed_model, weak_below).await?;
        // Indexed once, not scanned per member. Resolving with a linear `find`
        // and then a `retain` over the whole list cost two passes over every
        // open gap for every member of every cluster — a million moves at the
        // cap, on the page the app opens on.
        let index: std::collections::HashMap<(GapKind, &str), &Gap> =
            open.iter().map(|g| ((g.kind, g.id.as_str()), g)).collect();
        let mut clustered: std::collections::HashSet<(GapKind, &str)> = Default::default();
        let mut rows = Vec::new();
        for r in sqlx::query(
            "SELECT label, labelled_by, members FROM gap_clusters ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?
        {
            let members: Vec<serde_json::Value> =
                serde_json::from_str(&r.get::<String, _>("members"))
                    .map_err(|e| Error::Internal(e.to_string()))?;
            let mut resolved = Vec::new();
            for m in &members {
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
                    label: r.get("label"),
                    labelled_by: r.get("labelled_by"),
                    members: resolved,
                });
            }
        }
        // Newest first, the order the query returned: a gap judged this morning
        // is the one someone is still trying to fill.
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
                scope: None,
                filters: "{}".into(),
                query_vec: vec,
                embed_model: "fake".into(),
                answer: "Not in the knowledge base.".into(),
                abstained: true,
                dropped: 0,
                truncated: false,
                citations: vec![],
            })
            .await
            .unwrap();
        store.judge_ask(&id, AskVerdict::NothingHere).await.unwrap();
        id
    }

    async fn gap_search(store: &Store, q: &str, vec: Vec<f32>) -> String {
        let id = store
            .record_search(
                NewEvent {
                    query: q.into(),
                    door: Door::Api,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec,
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                },
                0,
            )
            .await
            .unwrap();
        store.judge(&id, Verdict::Gap).await.unwrap();
        id
    }

    /// A recorded search with candidates at the given similarities. No verdict:
    /// nobody judged it, which is the case `Unmatched` exists for.
    async fn search_with(store: &Store, q: &str, sims: &[f32]) -> String {
        store
            .record_search(
                NewEvent {
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
                        })
                        .collect(),
                    answered: false,
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
        // One hit above the line is enough: something came close, and the base
        // is not being asked about a hole.
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
    async fn a_search_that_measured_nothing_is_not_a_gap() {
        // `MAX(...)` over no candidate rows is NULL, and NULL is not under the
        // line. "Nothing came close" is a claim about what was measured.
        let store = Store::memory().await.unwrap();
        search_with(&store, "mount an E01", &[]).await;

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
        // The same row, said twice, in two different words.
        let store = Store::memory().await.unwrap();
        let id = search_with(&store, "mount an E01", &[0.10]).await;
        store.judge(&id, Verdict::Gap).await.unwrap();

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
        // `discard` is the operator saying this was never a search — a typo, or
        // poking at the box. A typo's candidates are exactly what scores below
        // the line, so a rule that only skipped `gap` would take every judgement
        // of "this was nothing" and put it straight back on the capture page as
        // a hole in the base. A `hit` whose candidates all scored weakly is the
        // other half of the same mistake: the ranking was poor, the knowledge
        // was there.
        let store = Store::memory().await.unwrap();
        let typo = search_with(&store, "mont an E01", &[0.10]).await;
        store.judge(&typo, Verdict::Discard).await.unwrap();
        let weak_hit = search_with(&store, "grep a pcap", &[0.12]).await;
        store.judge(&weak_hit, Verdict::Hit).await.unwrap();

        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;

        assert!(
            gaps.iter().all(|g| g.gap.kind != GapKind::Unmatched),
            "a judged search is settled, whatever the judgement was: {:?}",
            gaps.iter().map(|g| &g.gap.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn the_gaps_a_page_of_captures_answered_come_back_grouped() {
        // The queue fragment is polled while anything is in flight, and asking
        // per row turns one three-way join into a round of them.
        let store = Store::memory().await.unwrap();
        // `gap_coverage` names both a capture and the artifact of it that
        // answered, and both are foreign keys.
        for (c, a) in [("corpus-a", "art-1"), ("corpus-b", "art-2")] {
            sqlx::query(
                "INSERT INTO corpora (id, raw_text, origin, content_hash, status,
                                      created_at, updated_at)
                 VALUES (?, 'text', 'paste', ?, 'ready', 0, 0)",
            )
            .bind(c)
            .bind(c)
            .execute(&store.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO artifacts (id, corpus_id, ordinal, text, created_at)
                 VALUES (?, ?, 0, 'text', 0)",
            )
            .bind(a)
            .bind(c)
            .execute(&store.pool)
            .await
            .unwrap();
        }
        let one = search_with(&store, "mount an E01", &[0.10]).await;
        let two = search_with(&store, "grep a pcap", &[0.10]).await;
        store
            .cover_gap(GapKind::Unmatched, &one, "corpus-a", "art-1", 0.8)
            .await
            .unwrap();
        store
            .cover_gap(GapKind::Unmatched, &two, "corpus-b", "art-2", 0.9)
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
        // And the single-capture reader is the same answer.
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
        // A pursuit that got its answer is not a hole in the base.
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
    async fn a_pursuit_written_before_the_vector_was_carried_is_not_a_gap() {
        // Nothing to group it by. The same rule `vec_dim > 0` already applies
        // to a search whose vector the cache had evicted: an uncomparable
        // vector is left out rather than compared anyway.
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
        // Not gaps: a right answer, an unjudged search, an empty vector.
        let right = store
            .record_ask(NewAsk {
                question: "ok".into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec![1.0, 1.0],
                embed_model: "fake".into(),
                answer: "yes".into(),
                abstained: false,
                dropped: 0,
                truncated: false,
                citations: vec![],
            })
            .await
            .unwrap();
        store.judge_ask(&right, AskVerdict::Right).await.unwrap();
        nothing_here(&store, "no vector", vec![]).await;
        let gaps = store.open_gaps("fake", 0.35).await.unwrap().gaps;
        // Newest first across both kinds. Judged inside the same second, so the
        // uuid v7 ids break the tie — and being time-ordered they break it the
        // same way the stamps would have: the search was recorded second.
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

        // The display path reads the same gaps and no vectors.
        let refs = store.open_gap_refs("fake", 0.35).await.unwrap();
        assert_eq!(
            refs.iter().map(|g| g.text.as_str()).collect::<Vec<_>>(),
            vec!["q1", "s1"]
        );
    }

    /// The sweep compares every pair of open gaps and the capture page walks
    /// the same list on every load, so the number of them has to be bounded
    /// somewhere. Newest first, so what a cap drops is the oldest.
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
    }

    #[tokio::test]
    async fn a_table_holding_exactly_the_cap_has_left_nothing_out() {
        // Reading `MAX_OPEN_GAPS` rows and calling that capped cannot tell a
        // full page from a truncated one, and `sweep` now keeps every group it
        // could not reach when the pass was partial — so at exactly the cap a
        // base would stop cleaning up stale groups altogether, for good.
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

        // Dismissing a member thins the row; dismissing both removes it.
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
