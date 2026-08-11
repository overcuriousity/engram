//! What a real search looked like, so it can be judged later.
//!
//! The query is the one thing no amount of care can reconstruct afterwards: it
//! has to be recorded in the moment, before any result was seen. The verdict is
//! the opposite — it needs a person, and it can wait. Everything here exists to
//! keep those two apart in time, because a label assigned while reading the
//! answer contaminates the question.

use super::{Store, new_id, now};
use crate::error::Result;
use sqlx::Row;

/// Which front door a search came through.
///
/// An explicit parameter rather than a field on `SearchQuery`: that struct is
/// deserialised from the query string, so a `Default` there would silently
/// record an API search as a UI search the first time a caller forgot to set
/// it. With no default, the compiler asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Ui,
    Api,
    Mcp,
    /// The search inside the judging view's "none of these" path. Never
    /// captured: those queries are composed in full knowledge of the answer,
    /// which is exactly the contamination this whole feature exists to avoid.
    Judge,
    /// The retrieval behind `ask`. Never captured either, for a different
    /// reason: its right answer is a synthesis across several artifacts, so
    /// "which one was it" has no well-defined meaning to judge.
    Ask,
}

impl Door {
    pub fn as_str(&self) -> &'static str {
        match self {
            Door::Ui => "ui",
            Door::Api => "api",
            Door::Mcp => "mcp",
            Door::Judge => "judge",
            Door::Ask => "ask",
        }
    }

    pub fn captured(&self) -> bool {
        matches!(self, Door::Ui | Door::Api | Door::Mcp)
    }
}

#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub artifact_id: String,
    pub score: f32,
    /// Cosine, where the store could report one. `None` for a hit the lexical
    /// half matched verbatim — see `SearchResult::weak`.
    pub similarity: Option<f32>,
    /// Whether it was inside the answer the searcher actually saw.
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub query: String,
    pub door: Door,
    /// JSON, so a replay can reproduce the same narrowing.
    pub filters: String,
    pub query_vec: Vec<f32>,
    /// Vectors are only comparable under the model that produced them.
    pub embed_model: String,
    pub candidates: Vec<NewCandidate>,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl Store {
    /// Record one search, folding a typing burst into a single event.
    ///
    /// Capturing only deliberate searches would lose the most valuable case:
    /// `mark` is set on open, expand and submit, so a search where the operator
    /// found nothing useful and gave up would never be recorded. So everything
    /// is captured, and an event whose query strictly extends the previous one
    /// from the same door, within `coalesce_secs`, replaces it. What survives is
    /// the final wording — the query that was actually meant.
    pub async fn record_search(&self, ev: NewEvent, coalesce_secs: i64) -> Result<String> {
        let mut tx = self.pool.begin().await?;
        let at = now();

        let prev = sqlx::query(
            "SELECT id, query, created_at FROM search_events
             WHERE door = ? AND judged_at IS NULL
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(ev.door.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let extends = prev.as_ref().and_then(|r| {
            let prior: String = r.get("query");
            let created: i64 = r.get("created_at");
            // A window of zero means folding is off, not "fold within the same
            // second" — which is what a plain `<=` gives, since both events
            // usually land on one timestamp.
            let fresh = coalesce_secs > 0 && at - created <= coalesce_secs;
            let grew = ev.query.len() > prior.len() && ev.query.starts_with(&prior);
            (fresh && grew).then(|| r.get::<String, _>("id"))
        });

        let id = match extends {
            Some(id) => {
                sqlx::query(
                    "UPDATE search_events
                     SET query = ?, filters = ?, query_vec = ?, vec_dim = ?,
                         embed_model = ?, created_at = ?
                     WHERE id = ?",
                )
                .bind(&ev.query)
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM search_candidates WHERE event_id = ?")
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                id
            }
            None => {
                let id = new_id();
                sqlx::query(
                    "INSERT INTO search_events
                       (id, query, door, filters, query_vec, vec_dim, embed_model, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&ev.query)
                .bind(ev.door.as_str())
                .bind(&ev.filters)
                .bind(vec_to_blob(&ev.query_vec))
                .bind(ev.query_vec.len() as i64)
                .bind(&ev.embed_model)
                .bind(at)
                .execute(&mut *tx)
                .await?;
                id
            }
        };

        for (rank, c) in ev.candidates.iter().enumerate() {
            sqlx::query(
                "INSERT INTO search_candidates
                   (event_id, rank, artifact_id, score, similarity, shown)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(rank as i64)
            .bind(&c.artifact_id)
            .bind(c.score)
            .bind(c.similarity)
            .bind(c.shown as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// One artifact is the answer. `expect_id` names it — and it may be an
    /// artifact the search never returned, which is the most valuable case
    /// there is.
    Hit,
    /// Nothing in the base could have answered this. Not a pair; a finding.
    Gap,
    /// Not a real search — a typo, or poking at the box.
    Discard,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Hit => "hit",
            Verdict::Gap => "gap",
            Verdict::Discard => "discard",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub artifact_id: String,
    pub rank: i64,
    pub shown: bool,
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: String,
    pub query: String,
    pub door: String,
    pub created_at: i64,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub captured: i64,
    pub pending: i64,
    pub judged: i64,
    pub hits: i64,
    /// Hits whose artifact the search never returned. Rare, expensive, and the
    /// only evidence that ranking — rather than the corpus — was at fault.
    pub finds: i64,
    pub gaps: i64,
    pub discards: i64,
    pub recall_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone)]
pub struct Miss {
    pub query: String,
    /// `None` means the confirmed artifact was not in the stored pool at all.
    pub rank: Option<i64>,
}

impl Store {
    /// The next event to judge: never-skipped first, newest first within that.
    ///
    /// Newest first because a judgement is worth something only while the
    /// situation is still in mind, and that memory is the most perishable part
    /// of the whole dataset.
    pub async fn next_pending(&self) -> Result<Option<PendingEvent>> {
        let row = sqlx::query(
            // `id DESC` breaks the tie: two searches within one second are
            // ordinary, and `created_at` alone would leave SQLite to pick.
            // Ids are uuid v7, so they sort by time down to the millisecond.
            "SELECT id, query, door, created_at FROM search_events
             WHERE judged_at IS NULL
             ORDER BY skips ASC, created_at DESC, id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let id: String = row.get("id");

        let candidates = sqlx::query(
            "SELECT artifact_id, rank, shown FROM search_candidates
             WHERE event_id = ? ORDER BY rank",
        )
        .bind(&id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Candidate {
            artifact_id: r.get("artifact_id"),
            rank: r.get("rank"),
            shown: r.get::<i64, _>("shown") == 1,
        })
        .collect();

        Ok(Some(PendingEvent {
            id,
            query: row.get("query"),
            door: row.get("door"),
            created_at: row.get("created_at"),
            candidates,
        }))
    }

    /// Where this artifact stood in what the search returned, if it was in the
    /// pool at all. `None` is the interesting answer: it means the search never
    /// offered what turned out to be the right thing.
    pub async fn rank_in_event(&self, event_id: &str, artifact_id: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT rank FROM search_candidates WHERE event_id = ? AND artifact_id = ?",
        )
        .bind(event_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn judge_hit(&self, event_id: &str, artifact_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE search_events SET judged_at = ?, verdict = 'hit', expect_id = ?
             WHERE id = ?",
        )
        .bind(now())
        .bind(artifact_id)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn judge(&self, event_id: &str, verdict: Verdict) -> Result<()> {
        sqlx::query("UPDATE search_events SET judged_at = ?, verdict = ? WHERE id = ?")
            .bind(now())
            .bind(verdict.as_str())
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Not a verdict: the event stays pending and only sinks in the order. An
    /// honest "I don't remember" must never cost anything, or it stops being
    /// honest.
    pub async fn skip_event(&self, event_id: &str) -> Result<()> {
        sqlx::query("UPDATE search_events SET skips = skips + 1 WHERE id = ?")
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The field value: recall@10 and MRR read from the ranks the searches
    /// actually gave. No vector store and no embedding are involved, so the
    /// number can move on every single judgement — which is what makes it worth
    /// showing while judging rather than afterwards.
    pub async fn feedback_stats(&self) -> Result<Stats> {
        let mut s = Stats {
            captured: sqlx::query_scalar("SELECT count(*) FROM search_events")
                .fetch_one(&self.pool)
                .await?,
            pending: sqlx::query_scalar(
                "SELECT count(*) FROM search_events WHERE judged_at IS NULL",
            )
            .fetch_one(&self.pool)
            .await?,
            ..Default::default()
        };

        for (field, verdict) in [
            (&mut s.hits, "hit"),
            (&mut s.gaps, "gap"),
            (&mut s.discards, "discard"),
        ] {
            *field = sqlx::query_scalar("SELECT count(*) FROM search_events WHERE verdict = ?")
                .bind(verdict)
                .fetch_one(&self.pool)
                .await?;
        }
        s.judged = s.hits + s.gaps + s.discards;

        // A left join, because an expected artifact that was never returned has
        // no candidate row to join to — and that absence is precisely what a
        // miss is.
        let ranks: Vec<Option<i64>> = sqlx::query(
            "SELECT c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit'",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| r.get::<Option<i64>, _>("rank"))
        .collect();

        s.finds = ranks.iter().filter(|r| r.is_none()).count() as i64;
        if !ranks.is_empty() {
            let n = ranks.len() as f64;
            s.recall_at_10 = ranks
                .iter()
                .filter(|r| matches!(r, Some(i) if *i < 10))
                .count() as f64
                / n;
            s.mrr = ranks
                .iter()
                .map(|r| r.map_or(0.0, |i| 1.0 / (i as f64 + 1.0)))
                .sum::<f64>()
                / n;
        }
        Ok(s)
    }

    /// The queries whose confirmed answer fell outside the first ten. The list
    /// that is actually read: an aggregate says something is wrong, this says
    /// what.
    pub async fn misses(&self, limit: i64) -> Result<Vec<Miss>> {
        Ok(sqlx::query(
            "SELECT e.query AS query, c.rank AS rank FROM search_events e
             LEFT JOIN search_candidates c
               ON c.event_id = e.id AND c.artifact_id = e.expect_id
             WHERE e.verdict = 'hit' AND (c.rank IS NULL OR c.rank >= 10)
             ORDER BY e.judged_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Miss {
            query: r.get("query"),
            rank: r.get("rank"),
        })
        .collect())
    }

    /// Drop captured searches older than the window. `0` keeps them forever.
    ///
    /// Ridden along with the consolidation sweep rather than given a ticker of
    /// its own: a periodic `DELETE` is not worth a second moving part.
    pub async fn expire_feedback(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(
            sqlx::query("DELETE FROM search_events WHERE created_at < ?")
                .bind(now() - retain_days * 86_400)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Everything captured, gone. Judgements included: they are statements
    /// about queries, and a judgement whose query no longer exists is not a
    /// record of anything.
    pub async fn purge_feedback(&self) -> Result<u64> {
        // `search_candidates` goes with it through ON DELETE CASCADE.
        Ok(sqlx::query("DELETE FROM search_events")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(query: &str, door: Door) -> NewEvent {
        NewEvent {
            query: query.into(),
            door,
            filters: "{}".into(),
            query_vec: vec![0.5, -0.25],
            embed_model: "fake".into(),
            candidates: vec![NewCandidate {
                artifact_id: "a1".into(),
                score: 0.9,
                similarity: Some(0.8),
                shown: true,
            }],
        }
    }

    async fn queries(store: &Store) -> Vec<String> {
        sqlx::query("SELECT query FROM search_events ORDER BY created_at")
            .fetch_all(&store.pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("query"))
            .collect()
    }

    #[tokio::test]
    async fn a_typing_burst_collapses_to_its_final_wording() {
        let store = Store::memory().await.unwrap();
        for q in ["daten", "datentr", "datenträger nicht erkannt"] {
            store.record_search(ev(q, Door::Ui), 15).await.unwrap();
        }
        assert_eq!(queries(&store).await, vec!["datenträger nicht erkannt"]);
    }

    #[tokio::test]
    async fn a_query_that_is_not_a_prefix_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        store.record_search(ev("ntfs", Door::Ui), 15).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat32", "ntfs"]);
    }

    #[tokio::test]
    async fn a_prefix_from_another_door_does_not_fold_into_this_one() {
        // Two front doors are two people as far as this is concerned. Folding
        // an MCP call into a half-typed UI query would invent a search nobody
        // made.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        store
            .record_search(ev("fat32", Door::Mcp), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn a_prefix_outside_the_window_starts_its_own_event() {
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        // Zero window: the previous event is already too old to extend.
        store.record_search(ev("fat32", Door::Ui), 0).await.unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn folding_replaces_the_candidate_list_rather_than_appending_to_it() {
        // The candidates belong to the query that produced them. Keeping the
        // earlier ones would describe a result list that was never shown.
        let store = Store::memory().await.unwrap();
        store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let mut second = ev("fat32", Door::Ui);
        second.candidates[0].artifact_id = "a2".into();
        store.record_search(second, 15).await.unwrap();

        let rows = sqlx::query("SELECT artifact_id FROM search_candidates")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<String, _>("artifact_id"), "a2");
    }

    #[tokio::test]
    async fn a_judged_event_is_never_folded_into() {
        // Folding rewrites the query. Doing that under a verdict would leave a
        // label attached to words the operator never judged.
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        sqlx::query("UPDATE search_events SET judged_at = ?, verdict = 'gap' WHERE id = ?")
            .bind(now())
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .record_search(ev("fat32", Door::Ui), 15)
            .await
            .unwrap();
        assert_eq!(queries(&store).await, vec!["fat", "fat32"]);
    }

    #[tokio::test]
    async fn an_empty_result_list_is_still_captured() {
        // A search that found nothing is the most direct evidence of a gap the
        // system will ever get. It has no candidates and must still be stored.
        let store = Store::memory().await.unwrap();
        let mut e = ev("etwas das es nicht gibt", Door::Ui);
        e.candidates.clear();
        store.record_search(e, 15).await.unwrap();
        assert_eq!(queries(&store).await.len(), 1);
    }

    async fn seed(store: &Store, query: &str, ranked: &[&str]) -> String {
        let mut e = ev(query, Door::Ui);
        e.candidates = ranked
            .iter()
            .enumerate()
            .map(|(i, id)| NewCandidate {
                artifact_id: (*id).into(),
                score: 1.0 - i as f32 / 100.0,
                similarity: Some(0.5),
                shown: i < 10,
            })
            .collect();
        // No folding: these are separate searches, not one being typed.
        store.record_search(e, 0).await.unwrap()
    }

    #[tokio::test]
    async fn the_newest_unjudged_event_comes_up_first() {
        // Judging is worth something because the situation is still in mind,
        // and that memory is the most perishable part of the dataset.
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        seed(&store, "newer", &["b"]).await;
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "newer");
    }

    #[tokio::test]
    async fn a_skipped_event_sinks_below_the_ones_never_looked_at() {
        let store = Store::memory().await.unwrap();
        seed(&store, "older", &["a"]).await;
        let newer = seed(&store, "newer", &["b"]).await;
        store.skip_event(&newer).await.unwrap();
        assert_eq!(store.next_pending().await.unwrap().unwrap().query, "older");
    }

    #[tokio::test]
    async fn a_judged_event_does_not_come_back() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "only one", &["a"]).await;
        store.judge_hit(&id, "a").await.unwrap();
        assert!(store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_field_metrics_read_the_rank_the_search_actually_gave() {
        // No Qdrant and no embedding: the rank of every candidate was stored
        // when the search happened, so confirming one settles its rank too.
        let store = Store::memory().await.unwrap();
        let first = seed(&store, "top hit", &["a", "b", "c"]).await;
        store.judge_hit(&first, "a").await.unwrap();
        let third = seed(&store, "third hit", &["x", "y", "z"]).await;
        store.judge_hit(&third, "z").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.judged, 2);
        assert_eq!(s.hits, 2);
        assert!((s.recall_at_10 - 1.0).abs() < 1e-9);
        // 1/1 and 1/3, averaged.
        assert!((s.mrr - (1.0 + 1.0 / 3.0) / 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_answer_outside_the_pool_counts_as_a_find_and_a_miss() {
        // The whole point of the "none of these" path: an artifact the ranker
        // never returned. It has no rank, so it contributes nothing to MRR and
        // it drags recall down — which is the truth about that search.
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "found nothing useful", &["a", "b"]).await;
        store.judge_hit(&id, "something-else").await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!(s.finds, 1);
        assert_eq!(s.recall_at_10, 0.0);
        assert_eq!(s.mrr, 0.0);
        assert_eq!(store.misses(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gaps_and_discards_are_counted_but_are_not_pairs() {
        let store = Store::memory().await.unwrap();
        let g = seed(&store, "nothing written about this", &[]).await;
        store.judge(&g, Verdict::Gap).await.unwrap();
        let d = seed(&store, "asdf", &["a"]).await;
        store.judge(&d, Verdict::Discard).await.unwrap();

        let s = store.feedback_stats().await.unwrap();
        assert_eq!((s.gaps, s.discards, s.hits), (1, 1, 0));
        // Neither can score: one has no answer, the other was not a question.
        assert_eq!(s.mrr, 0.0);
    }

    #[tokio::test]
    async fn retention_of_zero_keeps_everything() {
        let store = Store::memory().await.unwrap();
        seed(&store, "old", &["a"]).await;
        assert_eq!(store.expire_feedback(0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_event_past_the_retention_window_is_dropped() {
        let store = Store::memory().await.unwrap();
        let id = seed(&store, "old", &["a"]).await;
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(now() - 40 * 86_400)
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.expire_feedback(30).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn purging_removes_events_and_their_candidates() {
        let store = Store::memory().await.unwrap();
        seed(&store, "a search", &["a", "b"]).await;
        store.purge_feedback().await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM search_candidates")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(store.next_pending().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_query_vector_survives_a_round_trip_through_the_blob() {
        let store = Store::memory().await.unwrap();
        let id = store.record_search(ev("fat", Door::Ui), 15).await.unwrap();
        let row = sqlx::query("SELECT query_vec, vec_dim FROM search_events WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("vec_dim"), 2);
        assert_eq!(
            blob_to_vec(&row.get::<Vec<u8>, _>("query_vec")),
            vec![0.5, -0.25]
        );
    }
}
