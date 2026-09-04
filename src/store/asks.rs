//! What a question looked like, so it can be judged later.
//!
//! The search side keeps the query and the verdict apart in time; a question
//! is judged where it is answered, because judging an answer means reading it
//! in context. What still has to be recorded in the moment is the answer and
//! the excerpts the model saw — the verdict is about *those*, and neither can
//! be reconstructed afterwards.

use super::{Store, new_id, now};
use crate::error::{Error, Result};
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct NewAskCitation {
    pub artifact_id: String,
    pub score: f32,
    /// The answer referenced this `[n]`. What the model was *shown* is not
    /// what it used, and only what it used is engagement — see
    /// `RecordedAsk::cited`.
    pub used: bool,
}

#[derive(Debug, Clone)]
pub struct NewAsk {
    pub question: String,
    /// The authenticated subject. Recorded, never used for coalescing: a
    /// question is one deliberate act, not a typing burst.
    pub scope: Option<String>,
    /// JSON, as `search_events.filters` is.
    pub filters: String,
    /// The vector ask retrieved with. May be empty when the query cache had
    /// already evicted it; such an event is never clustered as a gap.
    pub query_vec: Vec<f32>,
    pub embed_model: String,
    pub answer: String,
    pub abstained: bool,
    pub dropped: usize,
    pub truncated: bool,
    /// How many literals the answer carried that no excerpt it was shown
    /// held — the count behind the badge, not the strings.
    ///
    /// A count because one observation is written per answer rather than per
    /// literal: three unsupported literals are one retrieval that fell short,
    /// not three.
    pub unsupported: usize,
    /// In the order the model saw them; `n` is assigned 1-based from it.
    pub citations: Vec<NewAskCitation>,
}

/// One recorded question as the pursuit sweep reads it.
#[derive(Debug, Clone)]
pub struct RecordedAsk {
    pub id: String,
    pub question: String,
    pub query_vec: Vec<f32>,
    pub created_at: i64,
    pub abstained: bool,
    /// The excerpts the answer actually referenced, in the order they were
    /// shown. Not everything the model saw: an ask packs whatever fits, so
    /// "was in the prompt" says nothing about whether it helped, and an
    /// abstention leaves this empty however much it was given.
    pub cited: Vec<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskVerdict {
    /// The answer is correct as stated.
    Right,
    /// The base holds the answer and this is not it.
    Wrong,
    /// The base does not hold the answer, whatever the model said.
    NothingHere,
}

impl AskVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            AskVerdict::Right => "right",
            AskVerdict::Wrong => "wrong",
            AskVerdict::NothingHere => "nothing_here",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "right" => Some(AskVerdict::Right),
            "wrong" => Some(AskVerdict::Wrong),
            "nothing_here" => Some(AskVerdict::NothingHere),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AskCitation {
    pub n: i64,
    pub artifact_id: String,
    pub score: f32,
    pub carried: bool,
}

#[derive(Debug, Clone)]
pub struct AskEvent {
    pub id: String,
    pub question: String,
    pub answer: String,
    pub abstained: bool,
    pub verdict: Option<AskVerdict>,
    pub judged_at: Option<i64>,
    pub citations: Vec<AskCitation>,
}

#[derive(Debug, Clone, Default)]
pub struct AskStats {
    pub asked: i64,
    pub judged: i64,
    pub right: i64,
    pub wrong: i64,
    pub nothing_here: i64,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn one_row(res: sqlx::sqlite::SqliteQueryResult) -> Result<()> {
    if res.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

impl Store {
    pub async fn record_ask(&self, ask: NewAsk) -> Result<String> {
        // Read before the transaction opens: this is a different table with a
        // different lifetime, and holding the write lock across it would put a
        // question's own record behind a row that never changes.
        let generation = self.live_generation().await?;
        let mut tx = self.pool.begin().await?;
        let id = new_id();
        sqlx::query(
            "INSERT INTO ask_events
               (id, question, scope, filters, query_vec, vec_dim, embed_model, answer,
                abstained, dropped, truncated, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&ask.question)
        .bind(&ask.scope)
        .bind(&ask.filters)
        .bind(vec_to_blob(&ask.query_vec))
        .bind(ask.query_vec.len() as i64)
        .bind(&ask.embed_model)
        .bind(&ask.answer)
        .bind(ask.abstained as i64)
        .bind(ask.dropped as i64)
        .bind(ask.truncated as i64)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        for (i, c) in ask.citations.iter().enumerate() {
            sqlx::query(
                "INSERT INTO ask_citations (event_id, n, artifact_id, score, used)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(i as i64 + 1)
            .bind(&c.artifact_id)
            .bind(c.score)
            .bind(c.used as i64)
            .execute(&mut *tx)
            .await?;

            // The densest positive signal there is, and until now it was
            // computed, stored and read by nothing that tunes anything. An
            // excerpt the answer actually drew on says the retrieval that put
            // it there was right to.
            //
            // `abstained` is checked as well as `used`, though an abstention
            // references nothing and so has no used citation: the guard is
            // explicit because the rule is about the answer, not about the
            // scan that happens to implement it.
            if let Some(g) = &generation
                && c.used
                && !ask.abstained
            {
                crate::store::observations::insert(
                    &mut *tx,
                    &crate::store::observations::NewObservation {
                        generation_id: g.id.clone(),
                        query: ask.question.clone(),
                        query_vec: ask.query_vec.clone(),
                        embed_model: ask.embed_model.clone(),
                        artifact_id: Some(c.artifact_id.clone()),
                        rank: Some(i as i64 + 1),
                        source: crate::store::observations::Source::Cited,
                    },
                )
                .await?;
            }
        }

        // An answer that asserts a command or a path none of its excerpts held
        // is retrieval having failed to supply what the answer needed. It
        // names no artifact, because the claim is about the set: nothing in the
        // list was wrong to be there, the list was missing something.
        if let Some(g) = &generation
            && ask.unsupported > 0
        {
            crate::store::observations::insert(
                &mut *tx,
                &crate::store::observations::NewObservation {
                    generation_id: g.id.clone(),
                    query: ask.question.clone(),
                    query_vec: ask.query_vec.clone(),
                    embed_model: ask.embed_model.clone(),
                    artifact_id: None,
                    rank: None,
                    source: crate::store::observations::Source::Unsupported,
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn ask_event(&self, id: &str) -> Result<Option<AskEvent>> {
        let Some(row) = sqlx::query(
            "SELECT id, question, answer, abstained, verdict, judged_at FROM ask_events WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let citations = sqlx::query(
            "SELECT n, artifact_id, score, carried FROM ask_citations WHERE event_id = ? ORDER BY n",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| AskCitation {
            n: r.get("n"),
            artifact_id: r.get("artifact_id"),
            score: r.get::<f64, _>("score") as f32,
            carried: r.get::<i64, _>("carried") != 0,
        })
        .collect();
        Ok(Some(AskEvent {
            id: row.get("id"),
            question: row.get("question"),
            answer: row.get("answer"),
            abstained: row.get::<i64, _>("abstained") != 0,
            verdict: row
                .get::<Option<String>, _>("verdict")
                .as_deref()
                .and_then(AskVerdict::parse),
            judged_at: row.get("judged_at"),
            citations,
        }))
    }

    pub async fn judge_ask(&self, id: &str, verdict: AskVerdict) -> Result<()> {
        one_row(
            sqlx::query("UPDATE ask_events SET judged_at = ?, verdict = ? WHERE id = ?")
                .bind(now())
                .bind(verdict.as_str())
                .bind(id)
                .execute(&self.pool)
                .await?,
        )
    }

    /// Take a verdict back, carriers included. A carrier left behind would
    /// count towards citation recall for a judgement nobody stands behind.
    pub async fn unjudge_ask(&self, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let res =
            sqlx::query("UPDATE ask_events SET judged_at = NULL, verdict = NULL WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        one_row(res)?;
        sqlx::query("UPDATE ask_citations SET carried = 0 WHERE event_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Flip whether citation `n` carried the answer; returns the new state.
    /// Saying an excerpt carried the answer is saying the answer was right, so
    /// an unjudged event becomes `right`. A verdict already given is left as it
    /// is: the toggle refines a verdict, it does not overrule one.
    pub async fn toggle_carried(&self, id: &str, n: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            "UPDATE ask_citations SET carried = 1 - carried WHERE event_id = ? AND n = ?",
        )
        .bind(id)
        .bind(n)
        .execute(&mut *tx)
        .await?;
        one_row(res)?;
        let carried: i64 =
            sqlx::query_scalar("SELECT carried FROM ask_citations WHERE event_id = ? AND n = ?")
                .bind(id)
                .bind(n)
                .fetch_one(&mut *tx)
                .await?;
        if carried != 0 {
            sqlx::query(
                "UPDATE ask_events SET judged_at = ?, verdict = 'right'
                 WHERE id = ? AND verdict IS NULL",
            )
            .bind(now())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(carried != 0)
    }

    pub async fn ask_stats(&self) -> Result<AskStats> {
        let mut s = AskStats {
            asked: sqlx::query_scalar("SELECT count(*) FROM ask_events")
                .fetch_one(&self.pool)
                .await?,
            ..Default::default()
        };
        for (field, verdict) in [
            (&mut s.right, "right"),
            (&mut s.wrong, "wrong"),
            (&mut s.nothing_here, "nothing_here"),
        ] {
            *field = sqlx::query_scalar("SELECT count(*) FROM ask_events WHERE verdict = ?")
                .bind(verdict)
                .fetch_one(&self.pool)
                .await?;
        }
        s.judged = s.right + s.wrong + s.nothing_here;
        Ok(s)
    }

    /// Unjudged questions older than the window. Judged ones are exempt for
    /// the reason judged searches are: they are the operator's own work.
    pub async fn expire_asks(&self, retain_days: i64) -> Result<u64> {
        if retain_days <= 0 {
            return Ok(0);
        }
        Ok(
            sqlx::query("DELETE FROM ask_events WHERE created_at < ? AND verdict IS NULL")
                .bind(now() - retain_days * 86_400)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Recorded questions with `from < created_at <= to`, oldest first, with
    /// the excerpts their answers referenced — the sources a question engaged.
    pub async fn asks_between(&self, from: i64, to: i64) -> Result<Vec<RecordedAsk>> {
        let rows = sqlx::query(
            "SELECT id, question, query_vec, created_at, abstained, scope FROM ask_events
              WHERE created_at > ? AND created_at <= ? ORDER BY created_at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.get("id");
            let cited: Vec<String> = sqlx::query_scalar(
                "SELECT artifact_id FROM ask_citations
                  WHERE event_id = ? AND used = 1 ORDER BY n",
            )
            .bind(&id)
            .fetch_all(&self.pool)
            .await?;
            out.push(RecordedAsk {
                id,
                question: r.get("question"),
                query_vec: crate::store::feedback::blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                created_at: r.get("created_at"),
                abstained: r.get::<i64, _>("abstained") != 0,
                cited,
                scope: r.get("scope"),
            });
        }
        Ok(out)
    }

    pub async fn purge_asks(&self) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM ask_events")
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(question: &str, citations: usize) -> NewAsk {
        NewAsk {
            question: question.into(),
            scope: Some("me".into()),
            filters: "{}".into(),
            query_vec: vec![0.1, 0.2, 0.3],
            embed_model: "fake".into(),
            answer: "an answer".into(),
            abstained: false,
            dropped: 0,
            truncated: false,
            unsupported: 0,
            citations: (0..citations)
                .map(|i| NewAskCitation {
                    artifact_id: format!("art-{i}"),
                    score: 1.0 - i as f32 * 0.1,
                    used: true,
                })
                .collect(),
        }
    }

    /// A store with one generation live, so an observation has something to be
    /// evidence about.
    async fn base() -> (Store, String) {
        use crate::store::generations::{GenerationParams, NewGeneration};
        let store = Store::memory().await.unwrap();
        let generation = store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        (store, generation)
    }

    #[tokio::test]
    async fn a_used_citation_becomes_an_observation_at_the_rank_it_was_shown() {
        use crate::store::observations::Source;
        let (store, generation) = base().await;
        let mut a = ask("how did I mount it", 2);
        a.citations[0].used = false;
        store.record_ask(a).await.unwrap();

        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs.len(), 1, "only the used citation is an observation");
        assert_eq!(obs[0].artifact_id.as_deref(), Some("art-1"));
        assert_eq!(obs[0].rank, Some(2), "the [n] it was shown as");
        assert_eq!(obs[0].source, Source::Cited);
    }

    #[tokio::test]
    async fn an_abstention_leaves_no_observation_however_much_it_was_shown() {
        // Being packed into the prompt is not engagement. An abstention
        // references nothing however many excerpts it was given.
        let (store, generation) = base().await;
        let mut a = ask("something the base has never held", 3);
        a.abstained = true;
        store.record_ask(a).await.unwrap();

        assert!(
            store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_answer_asserting_what_no_excerpt_supports_is_a_negative_observation() {
        use crate::store::observations::Source;
        let (store, generation) = base().await;
        let mut a = ask("which flag was it", 1);
        a.unsupported = 2;
        store.record_ask(a).await.unwrap();

        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        let negative: Vec<_> = obs
            .iter()
            .filter(|o| o.source == Source::Unsupported)
            .collect();
        assert_eq!(
            negative.len(),
            1,
            "one observation per answer, not one per literal"
        );
        assert_eq!(
            negative[0].artifact_id, None,
            "the claim is about the set, not about anything in it"
        );
        assert!(negative[0].strength < 0.0);
    }

    #[tokio::test]
    async fn an_answer_whose_literals_were_all_supported_writes_no_negative() {
        use crate::store::observations::Source;
        let (store, generation) = base().await;
        let mut a = ask("which flag was it", 1);
        a.unsupported = 0;
        store.record_ask(a).await.unwrap();

        let obs = store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert!(obs.iter().all(|o| o.source != Source::Unsupported));
    }

    #[tokio::test]
    async fn an_ask_recorded_before_any_generation_exists_writes_no_observation() {
        // Ordering safety: the boot path that names a generation runs in the
        // background, and nothing may fail because it has not run yet.
        let store = Store::memory().await.unwrap();
        store.record_ask(ask("early", 2)).await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn a_recorded_ask_comes_back_with_its_citations_in_shown_order() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 3)).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().expect("recorded");
        assert_eq!(ev.question, "how");
        assert_eq!(ev.answer, "an answer");
        assert!(ev.verdict.is_none());
        assert_eq!(
            ev.citations
                .iter()
                .map(|c| (c.n, c.artifact_id.as_str(), c.carried))
                .collect::<Vec<_>>(),
            vec![
                (1, "art-0", false),
                (2, "art-1", false),
                (3, "art-2", false)
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_ask_is_none_not_an_error() {
        let store = Store::memory().await.unwrap();
        assert!(store.ask_event("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn judging_records_the_verdict_and_unjudging_takes_it_back_with_the_carriers() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 2)).await.unwrap();
        store.judge_ask(&id, AskVerdict::Wrong).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Wrong));
        assert!(ev.judged_at.is_some());

        assert!(store.toggle_carried(&id, 1).await.unwrap());
        store.unjudge_ask(&id).await.unwrap();
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert!(ev.verdict.is_none() && ev.judged_at.is_none());
        assert!(
            ev.citations.iter().all(|c| !c.carried),
            "a carrier left behind would count towards recall for a verdict nobody stands behind"
        );
    }

    #[tokio::test]
    async fn judging_an_unknown_ask_is_not_found() {
        let store = Store::memory().await.unwrap();
        assert!(matches!(
            store.judge_ask("nope", AskVerdict::Right).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            store.toggle_carried("nope", 1).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn marking_a_carrier_on_an_unjudged_ask_makes_it_right() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 2)).await.unwrap();
        assert!(store.toggle_carried(&id, 2).await.unwrap());
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Right));
        assert_eq!(
            ev.citations
                .iter()
                .filter(|c| c.carried)
                .map(|c| c.n)
                .collect::<Vec<_>>(),
            vec![2]
        );

        // Toggling again turns it off and leaves the verdict alone.
        assert!(!store.toggle_carried(&id, 2).await.unwrap());
        let ev = store.ask_event(&id).await.unwrap().unwrap();
        assert_eq!(ev.verdict, Some(AskVerdict::Right));
        assert!(ev.citations.iter().all(|c| !c.carried));
    }

    #[tokio::test]
    async fn a_carrier_on_a_wrong_answer_does_not_flip_the_verdict() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 1)).await.unwrap();
        store.judge_ask(&id, AskVerdict::Wrong).await.unwrap();
        store.toggle_carried(&id, 1).await.unwrap();
        assert_eq!(
            store.ask_event(&id).await.unwrap().unwrap().verdict,
            Some(AskVerdict::Wrong)
        );
    }

    #[tokio::test]
    async fn stats_count_what_was_asked_and_how_it_was_judged() {
        let store = Store::memory().await.unwrap();
        let a = store.record_ask(ask("a", 1)).await.unwrap();
        let b = store.record_ask(ask("b", 1)).await.unwrap();
        store.record_ask(ask("c", 1)).await.unwrap();
        store.judge_ask(&a, AskVerdict::Right).await.unwrap();
        store.judge_ask(&b, AskVerdict::NothingHere).await.unwrap();
        let s = store.ask_stats().await.unwrap();
        assert_eq!(
            (s.asked, s.judged, s.right, s.wrong, s.nothing_here),
            (3, 2, 1, 0, 1)
        );
    }

    #[tokio::test]
    async fn expiry_takes_unjudged_asks_past_the_window_and_keeps_judged_ones() {
        let store = Store::memory().await.unwrap();
        let old_unjudged = store.record_ask(ask("a", 1)).await.unwrap();
        let old_judged = store.record_ask(ask("b", 1)).await.unwrap();
        let fresh = store.record_ask(ask("c", 1)).await.unwrap();
        store
            .judge_ask(&old_judged, AskVerdict::Right)
            .await
            .unwrap();
        // Age two of them past a 30-day window.
        for id in [&old_unjudged, &old_judged] {
            sqlx::query("UPDATE ask_events SET created_at = ? WHERE id = ?")
                .bind(now() - 31 * 86_400)
                .bind(id)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        assert_eq!(store.expire_asks(30).await.unwrap(), 1);
        assert!(store.ask_event(&old_unjudged).await.unwrap().is_none());
        assert!(store.ask_event(&old_judged).await.unwrap().is_some());
        assert!(store.ask_event(&fresh).await.unwrap().is_some());
        // Zero means keep forever.
        assert_eq!(store.expire_asks(0).await.unwrap(), 0);
        // Citations go with the event.
        let orphans: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ask_citations WHERE event_id = ?")
                .bind(&old_unjudged)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn purge_takes_everything_judged_or_not() {
        let store = Store::memory().await.unwrap();
        let a = store.record_ask(ask("a", 1)).await.unwrap();
        store.record_ask(ask("b", 1)).await.unwrap();
        store.judge_ask(&a, AskVerdict::Right).await.unwrap();
        assert_eq!(store.purge_asks().await.unwrap(), 2);
        assert_eq!(store.ask_stats().await.unwrap().asked, 0);
    }

    #[tokio::test]
    async fn asks_between_carries_what_was_cited_and_whether_it_abstained() {
        let store = Store::memory().await.unwrap();
        let id = store.record_ask(ask("how", 2)).await.unwrap();
        let mut abst = ask("nothing", 0);
        abst.abstained = true;
        store.record_ask(abst).await.unwrap();
        let now = crate::store::now();
        let got = store.asks_between(0, now + 1).await.unwrap();
        assert_eq!(got.len(), 2);
        let first = got.iter().find(|a| a.id == id).unwrap();
        assert_eq!(first.cited, vec!["art-0".to_string(), "art-1".to_string()]);
        assert!(!first.abstained);
        assert_eq!(first.scope.as_deref(), Some("me"));
        assert!(got.iter().any(|a| a.abstained && a.cited.is_empty()));
        assert!(got.iter().all(|a| a.query_vec == vec![0.1, 0.2, 0.3]));
    }
}
