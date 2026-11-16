//! What the decision model said of each candidate of a recorded search, and how far that agrees
//! with the person. A label is never a verdict: nothing here writes `search_events`. A row with
//! `artifact_id = ''` marks a search that stepped aside (`label::step_aside`); no figure reads it.

use super::{Store, now};
use crate::error::Result;

/// Retrieval read off the model's labels. `searches` are the labelled searches with a relevant
/// candidate; `left_out` the labelled ones with none, which no figure here can score.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelRetrieval {
    pub searches: usize,
    pub left_out: usize,
    pub recall_at_10: f64,
    pub mrr: f64,
}

/// Over the searches a person judged a hit that the model labelled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Agreement {
    pub judged: usize,
    /// Of those, the searches whose expected note the model never labelled: each a miss.
    pub unlabelled: usize,
    pub hit_rate: f64,
    pub selectivity: f64,
}

/// Refusals after which a search is given up for its era. The queue backs a refusal off from half
/// a minute to six hours, so twenty of them span about two days: long enough for a wrong key to be
/// put right, short of four calls a day for ever.
pub const LABEL_GIVE_UP: i64 = 20;

/// Of the bearing notes in the decisions a person ruled on, how many held up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BearsRecord {
    pub held_up: usize,
    pub ruled_on: usize,
}

impl Store {
    /// Searches with a candidate still in results and no label from this model at this version,
    /// oldest first; not one the person called no search (`discard`). Only what can run now: not
    /// a search waiting out a failed attempt, and not one given up in this era.
    pub async fn unlabelled_searches(
        &self,
        version: u32,
        model: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT e.id AS "id!" FROM search_events e
                WHERE e.verdict IS NOT 'discard'
                  AND NOT EXISTS (SELECT 1 FROM search_labels l
                                   WHERE l.event_id = e.id AND l.question_version = ? AND l.model = ?)
                  AND NOT EXISTS (SELECT 1 FROM search_label_failures f
                                   WHERE f.event_id = e.id AND f.question_version = ? AND f.model = ?
                                     AND (f.refused >= ? OR f.retry_at > ?))
                  AND EXISTS (SELECT 1 FROM search_candidates c
                                JOIN artifacts a ON a.id = c.artifact_id
                               WHERE c.event_id = e.id
                                 AND a.status = 'active' AND a.superseded_by IS NULL)
                ORDER BY e.created_at ASC, e.id ASC LIMIT ?"#,
            version,
            model,
            version,
            model,
            LABEL_GIVE_UP,
            now(),
            limit
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// A failed attempt at labelling a search in this era: not due again before `retry_at`, and
    /// counted when the model `refused` it, which waiting does not fix. An older era's row starts
    /// over. Returns the refusals so far in this era.
    pub async fn label_failed(
        &self,
        event_id: &str,
        version: u32,
        model: &str,
        refused: bool,
        retry_at: i64,
    ) -> Result<i64> {
        let refused = i64::from(refused);
        sqlx::query!(
            "INSERT INTO search_label_failures (event_id, question_version, model, refused, retry_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(event_id) DO UPDATE SET
               refused = CASE WHEN question_version = excluded.question_version
                               AND model = excluded.model
                              THEN refused + excluded.refused ELSE excluded.refused END,
               question_version = excluded.question_version,
               model = excluded.model,
               retry_at = excluded.retry_at",
            event_id,
            version,
            model,
            refused,
            retry_at
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_scalar!(
            "SELECT refused FROM search_label_failures WHERE event_id = ?",
            event_id
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// A search's query and its candidates, each once, in pool order; `None` once retention took it.
    pub async fn search_to_label(&self, event_id: &str) -> Result<Option<(String, Vec<String>)>> {
        let Some(query) = self.search_query(event_id).await? else {
            return Ok(None);
        };
        let candidates = sqlx::query_scalar!(
            r#"SELECT artifact_id AS "artifact_id!" FROM search_candidates WHERE event_id = ?
                GROUP BY artifact_id ORDER BY MIN(rank)"#,
            event_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(Some((query, candidates)))
    }

    /// Replaces the search's labels: one era per search. Labelled, its failed attempts are done with.
    pub async fn put_search_labels(
        &self,
        event_id: &str,
        rows: &[(String, f64)],
        version: u32,
        model: &str,
    ) -> Result<()> {
        let at = now();
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM search_labels WHERE event_id = ?", event_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "DELETE FROM search_label_failures WHERE event_id = ?",
            event_id
        )
        .execute(&mut *tx)
        .await?;
        for (artifact_id, yes) in rows {
            sqlx::query!(
                "INSERT INTO search_labels (event_id, artifact_id, yes, question_version, model, at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                event_id,
                artifact_id,
                yes,
                version,
                model,
                at
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Per labelled search: recall@10 is the share of its relevant candidates in the first ten
    /// places, MRR the reciprocal place of the first. Places are the recorded pool's, from one. A
    /// search the person has since called no search (`discard`) is neither scored nor left out.
    pub async fn model_retrieval(
        &self,
        relevant_at: f64,
        version: u32,
        model: &str,
    ) -> Result<ModelRetrieval> {
        // A candidate in the pool and the band is placed at its pool rank, the lower.
        let rows = sqlx::query!(
            r#"SELECT l.event_id AS "event_id!", l.yes AS "yes!: f64",
                      (SELECT MIN(c.rank) FROM search_candidates c
                        WHERE c.event_id = l.event_id AND c.artifact_id = l.artifact_id) AS "rank!: i64"
                 FROM search_labels l
                WHERE l.question_version = ? AND l.model = ?
                  AND EXISTS (SELECT 1 FROM search_candidates c
                               WHERE c.event_id = l.event_id AND c.artifact_id = l.artifact_id)
                  AND EXISTS (SELECT 1 FROM search_events e
                               WHERE e.id = l.event_id AND e.verdict IS NOT 'discard')
                ORDER BY l.event_id, 3"#,
            version,
            model
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = ModelRetrieval::default();
        let (mut recall, mut mrr) = (0.0, 0.0);
        for search in rows.chunk_by(|a, b| a.event_id == b.event_id) {
            let relevant: Vec<i64> = search
                .iter()
                .filter(|r| r.yes >= relevant_at)
                .map(|r| r.rank)
                .collect();
            let Some(first) = relevant.first() else {
                out.left_out += 1;
                continue;
            };
            out.searches += 1;
            recall += relevant.iter().filter(|r| **r < 10).count() as f64 / relevant.len() as f64;
            mrr += 1.0 / (*first as f64 + 1.0);
        }
        if out.searches > 0 {
            out.recall_at_10 = recall / out.searches as f64;
            out.mrr = mrr / out.searches as f64;
        }
        Ok(out)
    }

    /// Over the searches a person judged a hit that the model labelled: the hit rate is the share
    /// whose expected note is relevant, the selectivity the mean share of a search's labelled
    /// candidates that are relevant. An expected note the model never labelled is a miss.
    pub async fn label_agreement(
        &self,
        relevant_at: f64,
        version: u32,
        model: &str,
    ) -> Result<Agreement> {
        let rows = sqlx::query!(
            r#"SELECT (SELECT x.yes FROM search_labels x
                        WHERE x.event_id = e.id AND x.artifact_id = e.expect_id
                          AND x.question_version = ? AND x.model = ?) AS "expect_yes?: f64",
                      (SELECT COUNT(*) FROM search_labels l
                        WHERE l.event_id = e.id AND l.question_version = ? AND l.model = ?
                          AND l.yes >= ? AND l.artifact_id <> '') AS "relevant!: i64",
                      (SELECT COUNT(*) FROM search_labels l
                        WHERE l.event_id = e.id AND l.question_version = ? AND l.model = ?
                          AND l.artifact_id <> '') AS "labelled!: i64"
                 FROM search_events e
                WHERE e.verdict = 'hit' AND e.expect_id IS NOT NULL
                  AND EXISTS (SELECT 1 FROM search_labels l
                               WHERE l.event_id = e.id AND l.question_version = ? AND l.model = ?
                                 AND l.artifact_id <> '')"#,
            version,
            model,
            version,
            model,
            relevant_at,
            version,
            model,
            version,
            model
        )
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(Agreement::default());
        }
        let n = rows.len() as f64;
        Ok(Agreement {
            judged: rows.len(),
            unlabelled: rows.iter().filter(|r| r.expect_yes.is_none()).count(),
            hit_rate: rows
                .iter()
                .filter(|r| r.expect_yes.is_some_and(|yes| yes >= relevant_at))
                .count() as f64
                / n,
            selectivity: rows
                .iter()
                .map(|r| r.relevant as f64 / r.labelled as f64)
                .sum::<f64>()
                / n,
        })
    }

    /// Over the held decisions made under this configured model and the wording of `bears` in
    /// force, judged `right` or `nothing_here`: every bearing note was ruled on; one held up when
    /// its verdict was `right` and its own reading picked the same.
    pub async fn bears_record(&self, model: &str) -> Result<BearsRecord> {
        use crate::core::decide::{standing::pick, wire};
        use crate::store::asks::AskVerdict;
        let ids = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM decide_events
                WHERE decider = ? AND not_held = 0 AND verdict IN ('right', 'nothing_here')
                  AND COALESCE(bears_version, 1) = ?"#,
            model,
            crate::core::decide::state::BEARS_VERSION
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = BearsRecord::default();
        for id in ids {
            let Some(d) = self.decision(&id).await? else {
                continue;
            };
            let bearing = d.evidence.iter().filter(|e| e.bearing);
            out.ruled_on += bearing.clone().count();
            if d.verdict != Some(AskVerdict::Right) {
                continue;
            }
            // A row this binary cannot read holds up nothing.
            let Some(q) = wire::stored_questions(&d.questions)
                .ok()
                .and_then(|qs| qs.into_iter().next())
            else {
                continue;
            };
            let Some(held) = d
                .answers
                .as_deref()
                .and_then(|a| wire::stored_answer(&q, a))
            else {
                continue;
            };
            let right = pick(&q, &held);
            out.held_up += bearing
                .filter(|e| wire::stored_answer(&q, &e.read).is_some_and(|a| pick(&q, &a) == right))
                .count();
        }
        Ok(out)
    }

    /// The candidate this model at this version rates highest for a search the person did not
    /// judge, if it is relevant.
    pub async fn model_expected(
        &self,
        event_id: &str,
        relevant_at: f64,
        version: u32,
        model: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT l.artifact_id AS "artifact_id!" FROM search_labels l
                 JOIN search_events e ON e.id = l.event_id
                WHERE l.event_id = ? AND e.judged_at IS NULL AND l.yes >= ? AND l.artifact_id <> ''
                  AND l.question_version = ? AND l.model = ?
                ORDER BY l.yes DESC,
                         (SELECT MIN(c.rank) FROM search_candidates c
                           WHERE c.event_id = l.event_id AND c.artifact_id = l.artifact_id) ASC
                LIMIT 1"#,
            event_id,
            relevant_at,
            version,
            model
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Unjudged searches under one generation that this model labelled at this version, newest
    /// first, with the model's expected note: what a sweep may use while `Core::labels_count`.
    pub async fn model_judged_pairs(
        &self,
        generation_id: &str,
        relevant_at: f64,
        version: u32,
        model: &str,
        limit: usize,
    ) -> Result<Vec<crate::store::feedback::JudgedPair>> {
        let rows = sqlx::query!(
            r#"SELECT e.id AS "id!", e.query AS "query!", e.query_vec, e.embed_model
                 FROM search_events e
                WHERE e.judged_at IS NULL AND e.generation_id = ?
                  AND EXISTS (SELECT 1 FROM search_labels l
                               WHERE l.event_id = e.id AND l.question_version = ? AND l.model = ?)
                ORDER BY e.created_at DESC, e.id DESC
                LIMIT ?"#,
            generation_id,
            version,
            model,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let Some(expect) = self
                .model_expected(&r.id, relevant_at, version, model)
                .await?
            else {
                continue;
            };
            let served_rank = sqlx::query_scalar!(
                "SELECT MIN(COALESCE(explored_from, rank)) + 1 FROM search_candidates
                  WHERE event_id = ? AND artifact_id = ?",
                r.id,
                expect
            )
            .fetch_one(&self.pool)
            .await?;
            out.push(crate::store::feedback::JudgedPair {
                event_id: r.id,
                query: r.query,
                expect,
                query_vec: crate::store::feedback::blob_to_vec(&r.query_vec),
                embed_model: r.embed_model,
                served_rank,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::feedback::{Door, Labeller, NewCandidate, NewEvent};

    const V: u32 = 1;
    const M: &str = "clef";

    pub(crate) async fn search(store: &Store, query: &str, candidates: &[String]) -> String {
        store
            .record_search(
                NewEvent {
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: candidates
                        .iter()
                        .map(|a| NewCandidate {
                            artifact_id: a.clone(),
                            score: 0.5,
                            similarity: Some(0.5),
                            shown: true,
                            ..Default::default()
                        })
                        .collect(),
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    fn ids(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}-{i}")).collect()
    }

    pub(crate) async fn notes(store: &Store, n: usize) -> Vec<String> {
        let src = store.insert_corpus("raw", "web", None).await.unwrap();
        let mut out = Vec::new();
        for i in 0..n {
            out.push(
                store
                    .insert_artifacts(
                        &src.id,
                        &[crate::store::artifacts::NewArtifact {
                            ordinal: i as i64,
                            text: format!("note {i}"),
                            ..Default::default()
                        }],
                    )
                    .await
                    .unwrap()
                    .remove(0)
                    .id,
            );
        }
        out
    }

    async fn aged(store: &Store, id: &str, at: i64) {
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            at,
            id
        )
        .execute(&store.pool)
        .await
        .unwrap();
    }

    /// Every candidate at `no`, but those at `yes_at` (0-based places) at 0.95.
    fn labels(candidates: &[String], yes_at: &[usize]) -> Vec<(String, f64)> {
        candidates
            .iter()
            .enumerate()
            .map(|(i, a)| (a.clone(), if yes_at.contains(&i) { 0.95 } else { 0.1 }))
            .collect()
    }

    #[tokio::test]
    async fn searches_are_labelled_oldest_first_and_once_per_version() {
        let store = Store::memory().await.unwrap();
        let n = notes(&store, 2).await;
        let new = search(&store, "newer", &n).await;
        let old = search(&store, "older", &n).await;
        aged(&store, &new, 2_000).await;
        aged(&store, &old, 1_000).await;
        // No candidate at all, and one whose candidates are all gone: nothing to label.
        search(&store, "empty", &[]).await;
        search(&store, "gone", &ids("missing", 2)).await;
        let discarded = search(&store, "asdf", &n).await;
        store
            .judge(
                &discarded,
                crate::store::feedback::Verdict::Discard,
                Labeller::Confirm,
            )
            .await
            .unwrap();

        assert_eq!(
            store.unlabelled_searches(V, M, 10).await.unwrap(),
            [old.clone(), new.clone()]
        );
        assert_eq!(
            store.unlabelled_searches(V, M, 1).await.unwrap(),
            std::slice::from_ref(&old)
        );
        store
            .put_search_labels(&old, &labels(&n, &[0]), V, M)
            .await
            .unwrap();
        assert_eq!(
            store.unlabelled_searches(V, M, 10).await.unwrap(),
            std::slice::from_ref(&new)
        );
        assert_eq!(
            store.unlabelled_searches(V + 1, M, 10).await.unwrap(),
            [old.clone(), new.clone()],
            "a new version labels again"
        );
        assert_eq!(
            store.unlabelled_searches(V, "other", 10).await.unwrap(),
            [old.clone(), new],
            "so does a new model"
        );
        // Labelled again by the next era: one era per search, the old rows gone.
        store
            .put_search_labels(&old, &labels(&n[..1], &[]), V + 1, M)
            .await
            .unwrap();
        let rows = sqlx::query!(
            "SELECT artifact_id, question_version FROM search_labels WHERE event_id = ?",
            old
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].question_version, i64::from(V + 1));
    }

    #[tokio::test]
    async fn model_recall_and_mrr_follow_the_definitions() {
        // 12 candidates, relevant at ranks 2, 5 and 11 -> recall@10 2/3, MRR 1/2
        let store = Store::memory().await.unwrap();
        let c = ids("a", 12);
        let id = search(&store, "loop device", &c).await;
        store
            .put_search_labels(&id, &labels(&c, &[1, 4, 10]), V, M)
            .await
            .unwrap();
        let r = store.model_retrieval(0.9, V, M).await.unwrap();
        assert_eq!((r.searches, r.left_out), (1, 0));
        assert!((r.recall_at_10 - 2.0 / 3.0).abs() < 1e-9, "{r:?}");
        assert!((r.mrr - 0.5).abs() < 1e-9, "{r:?}");
        assert_eq!(
            store.model_retrieval(0.9, V, "other").await.unwrap(),
            ModelRetrieval::default(),
            "another model's labels are not this one's"
        );
    }

    #[tokio::test]
    async fn a_search_with_nothing_relevant_is_left_out_and_counted() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 4);
        let scored = search(&store, "loop device", &c).await;
        let none = search(&store, "fat entries", &c).await;
        store
            .put_search_labels(&scored, &labels(&c, &[0]), V, M)
            .await
            .unwrap();
        store
            .put_search_labels(&none, &labels(&c, &[]), V, M)
            .await
            .unwrap();
        let r = store.model_retrieval(0.9, V, M).await.unwrap();
        assert_eq!((r.searches, r.left_out), (1, 1));
        assert!((r.recall_at_10 - 1.0).abs() < 1e-9);
        assert!(
            (r.mrr - 1.0).abs() < 1e-9,
            "a left-out search is not scored a miss"
        );
    }

    #[tokio::test]
    async fn hit_rate_and_selectivity_follow_the_definitions() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 10);
        // Expected note relevant, 2 of 10 relevant.
        let a = search(&store, "one", &c).await;
        store
            .put_search_labels(&a, &labels(&c, &[0, 1]), V, M)
            .await
            .unwrap();
        store.judge_hit(&a, &c[0], Labeller::Confirm).await.unwrap();
        // Expected note not relevant, 4 of 10 relevant.
        let b = search(&store, "two", &c).await;
        store
            .put_search_labels(&b, &labels(&c, &[0, 1, 2, 3]), V, M)
            .await
            .unwrap();
        store.judge_hit(&b, &c[9], Labeller::Confirm).await.unwrap();
        // Not judged, and judged but never labelled: neither counts.
        let u = search(&store, "three", &c).await;
        store
            .put_search_labels(&u, &labels(&c, &[0]), V, M)
            .await
            .unwrap();
        let bare = search(&store, "four", &c).await;
        store
            .judge_hit(&bare, &c[0], Labeller::Confirm)
            .await
            .unwrap();

        let g = store.label_agreement(0.9, V, M).await.unwrap();
        assert_eq!((g.judged, g.unlabelled), (2, 0));
        assert!((g.hit_rate - 0.5).abs() < 1e-9, "{g:?}");
        assert!((g.selectivity - 0.3).abs() < 1e-9, "{g:?}");
    }

    #[tokio::test]
    async fn an_expected_note_the_model_never_labelled_is_a_miss_not_a_search_left_out() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 10);
        // Expected note relevant, 2 of 10 relevant.
        let a = search(&store, "one", &c).await;
        store
            .put_search_labels(&a, &labels(&c, &[0, 1]), V, M)
            .await
            .unwrap();
        store.judge_hit(&a, &c[0], Labeller::Confirm).await.unwrap();
        // Labelled, 1 of 10 relevant, and the person found the answer in a note the model never saw.
        let f = search(&store, "two", &c).await;
        store
            .put_search_labels(&f, &labels(&c, &[0]), V, M)
            .await
            .unwrap();
        store
            .judge_hit(&f, "found-elsewhere", Labeller::Confirm)
            .await
            .unwrap();
        // A search that stepped aside carries no label any figure reads.
        let aside = search(&store, "three", &c).await;
        store
            .put_search_labels(&aside, &[(String::new(), 0.0)], V, M)
            .await
            .unwrap();
        store
            .judge_hit(&aside, &c[0], Labeller::Confirm)
            .await
            .unwrap();

        let g = store.label_agreement(0.9, V, M).await.unwrap();
        assert_eq!((g.judged, g.unlabelled), (2, 1));
        assert!((g.hit_rate - 0.5).abs() < 1e-9, "{g:?}");
        assert!((g.selectivity - 0.15).abs() < 1e-9, "{g:?}");
    }

    #[tokio::test]
    async fn a_model_that_says_yes_to_everything_has_a_perfect_hit_rate_and_no_selectivity() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 5);
        for i in 0..3 {
            let e = search(&store, &format!("q{i}"), &c).await;
            store
                .put_search_labels(&e, &labels(&c, &[0, 1, 2, 3, 4]), V, M)
                .await
                .unwrap();
            store.judge_hit(&e, &c[i], Labeller::Confirm).await.unwrap();
        }
        let g = store.label_agreement(0.9, V, M).await.unwrap();
        assert_eq!(
            g,
            Agreement {
                judged: 3,
                unlabelled: 0,
                hit_rate: 1.0,
                selectivity: 1.0
            }
        );
    }

    /// A held decision of model `M` judged `verdict`, one bearing note per pick in `reads`.
    pub(crate) async fn decided(
        store: &Store,
        verdict: crate::store::asks::AskVerdict,
        reads: &[&str],
    ) {
        use crate::core::decide::wire;
        use crate::infer::decide::{Answer, Kind, Question};
        use crate::store::decisions::{NewDecision, NewEvidence};
        let q = Question {
            key: "verdict".into(),
            kind: Kind::Choice {
                options: vec![
                    ("keep".into(), String::new()),
                    ("drop".into(), String::new()),
                ],
            },
            instructions: "Keep?".into(),
        };
        let qs = std::slice::from_ref(&q);
        let choose = |pick: &str| Answer::Choice {
            pick: pick.into(),
            probabilities: if pick == "keep" {
                vec![0.9, 0.1]
            } else {
                vec![0.1, 0.9]
            },
            confidence: 0.8,
        };
        let read = |pick: &str| wire::read_json(qs, &[(q.key.clone(), choose(pick))]).to_string();
        let evidence = reads
            .iter()
            .enumerate()
            .map(|(i, p)| NewEvidence {
                rank: i,
                artifact_id: format!("n{i}"),
                bears: 0.9,
                bearing: true,
                joint: true,
                n: Some(i),
                read: read(p),
            })
            // A passage that did not bear is never ruled on.
            .chain(std::iter::once(NewEvidence {
                rank: reads.len(),
                artifact_id: "quiet".into(),
                bears: 0.1,
                bearing: false,
                joint: false,
                n: None,
                read: read("keep"),
            }))
            .collect();
        let id = store
            .record_decision(NewDecision {
                question: "Keep it?".into(),
                questions: wire::questions_json(qs),
                situation: None,
                scope: None,
                filters: "{}".into(),
                answers: Some(
                    serde_json::json!({ "verdict": wire::answer_json(&q, &choose("keep"), Some(true)) })
                        .to_string(),
                ),
                prior: "{}".into(),
                standing: "{}".into(),
                not_held: false,
                model: "as-reported".into(),
                decider: M.into(),
                read: reads.len() + 1,
                dropped: 0,
                query_vec: vec![1.0],
                embed_model: "fake".into(),
                evidence,
            })
            .await
            .unwrap();
        store.judge_decision(&id, verdict, None).await.unwrap();
    }

    #[tokio::test]
    async fn bears_is_scored_on_the_notes_a_person_ruled_on() {
        /* one right decision with 3 agreeing of 4 bearing; one nothing_here with 2 bearing -> 3 of 6 */
        use crate::store::asks::AskVerdict;
        let store = Store::memory().await.unwrap();
        decided(&store, AskVerdict::Right, &["keep", "keep", "drop", "keep"]).await;
        decided(&store, AskVerdict::NothingHere, &["keep", "keep"]).await;
        // A wrong verdict is the reading's fault, not the bearing's: not counted.
        decided(&store, AskVerdict::Wrong, &["keep"]).await;
        assert_eq!(
            store.bears_record(M).await.unwrap(),
            BearsRecord {
                held_up: 3,
                ruled_on: 6
            }
        );
        assert_eq!(
            store.bears_record("other").await.unwrap(),
            BearsRecord::default()
        );
    }

    #[tokio::test]
    async fn bears_read_under_another_wording_is_not_scored_with_this_one() {
        use crate::store::asks::AskVerdict;
        let store = Store::memory().await.unwrap();
        decided(&store, AskVerdict::Right, &["keep", "keep", "keep"]).await;
        // What is there so far was read under the next wording.
        let next = i64::from(crate::core::decide::state::BEARS_VERSION) + 1;
        sqlx::query!("UPDATE decide_events SET bears_version = ?", next)
            .execute(&store.pool)
            .await
            .unwrap();
        decided(&store, AskVerdict::Right, &["keep", "drop"]).await;
        assert_eq!(
            store.bears_record(M).await.unwrap(),
            BearsRecord {
                held_up: 1,
                ruled_on: 2
            }
        );
    }

    #[tokio::test]
    async fn deleting_a_search_takes_its_labels() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 3);
        let id = search(&store, "loop device", &c).await;
        store
            .put_search_labels(&id, &labels(&c, &[0]), V, M)
            .await
            .unwrap();
        store.purge_feedback().await.unwrap();
        let left = sqlx::query_scalar!("SELECT count(*) FROM search_labels")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn the_model_expects_its_strongest_relevant_candidate_only_where_the_person_did_not_judge()
     {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 3);
        let id = search(&store, "loop device", &c).await;
        store
            .put_search_labels(
                &id,
                &[
                    (c[0].clone(), 0.92),
                    (c[1].clone(), 0.97),
                    (c[2].clone(), 0.2),
                ],
                V,
                M,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .model_expected(&id, 0.9, V, M)
                .await
                .unwrap()
                .as_deref(),
            Some(c[1].as_str())
        );
        assert_eq!(
            store.model_expected(&id, 0.98, V, M).await.unwrap(),
            None,
            "nothing relevant"
        );
        assert_eq!(
            store.model_expected(&id, 0.9, V + 1, M).await.unwrap(),
            None,
            "another wording's labels are not this one's"
        );
        assert_eq!(
            store.model_expected(&id, 0.9, V, "other").await.unwrap(),
            None,
            "nor another model's"
        );
        store
            .judge_hit(&id, &c[2], Labeller::Confirm)
            .await
            .unwrap();
        assert_eq!(
            store.model_expected(&id, 0.9, V, M).await.unwrap(),
            None,
            "the person's verdict wins"
        );
    }

    #[tokio::test]
    async fn a_search_discarded_after_it_was_labelled_is_not_scored() {
        let store = Store::memory().await.unwrap();
        let c = ids("a", 4);
        let kept = search(&store, "loop device", &c).await;
        let relevant = search(&store, "asdf", &c).await;
        let nothing = search(&store, "qwer", &c).await;
        store
            .put_search_labels(&kept, &labels(&c, &[1]), V, M)
            .await
            .unwrap();
        store
            .put_search_labels(&relevant, &labels(&c, &[0]), V, M)
            .await
            .unwrap();
        store
            .put_search_labels(&nothing, &labels(&c, &[]), V, M)
            .await
            .unwrap();
        for id in [&relevant, &nothing] {
            store
                .judge(
                    id,
                    crate::store::feedback::Verdict::Discard,
                    Labeller::Confirm,
                )
                .await
                .unwrap();
        }
        let r = store.model_retrieval(0.9, V, M).await.unwrap();
        assert_eq!((r.searches, r.left_out), (1, 0));
        assert!((r.mrr - 0.5).abs() < 1e-9, "{r:?}");
    }

    #[tokio::test]
    async fn a_search_waiting_out_a_failed_attempt_is_not_offered_and_does_not_hide_the_next() {
        let store = Store::memory().await.unwrap();
        let n = notes(&store, 1).await;
        let mut stuck = Vec::new();
        for i in 0..64 {
            let id = search(&store, &format!("stuck {i}"), &n).await;
            aged(&store, &id, 1_000 + i).await;
            store
                .label_failed(&id, V, M, false, now() + 3_600)
                .await
                .unwrap();
            stuck.push(id);
        }
        let healthy = search(&store, "healthy", &n).await;
        aged(&store, &healthy, 2_000).await;

        assert_eq!(
            store.unlabelled_searches(V, M, 64).await.unwrap(),
            std::slice::from_ref(&healthy),
            "the sixty-fifth is the only one that can run"
        );
        // Due again, it is offered again, oldest first.
        sqlx::query!(
            "UPDATE search_label_failures SET retry_at = 0 WHERE event_id = ?",
            &stuck[0]
        )
        .execute(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            store.unlabelled_searches(V, M, 64).await.unwrap(),
            [stuck[0].clone(), healthy]
        );
    }

    #[tokio::test]
    async fn a_search_refused_past_the_bound_is_given_up_until_the_era_changes() {
        let store = Store::memory().await.unwrap();
        let n = notes(&store, 1).await;
        let id = search(&store, "refused", &n).await;
        // A failure that waiting fixes is never counted, however often.
        for _ in 0..LABEL_GIVE_UP + 1 {
            assert_eq!(store.label_failed(&id, V, M, false, 0).await.unwrap(), 0);
        }
        for i in 1..LABEL_GIVE_UP {
            assert_eq!(store.label_failed(&id, V, M, true, 0).await.unwrap(), i);
        }
        assert_eq!(
            store.unlabelled_searches(V, M, 10).await.unwrap(),
            std::slice::from_ref(&id),
            "one short of the bound and due: still offered"
        );
        assert_eq!(
            store.label_failed(&id, V, M, true, 0).await.unwrap(),
            LABEL_GIVE_UP
        );
        assert!(
            store
                .unlabelled_searches(V, M, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.unlabelled_searches(V + 1, M, 10).await.unwrap(),
            std::slice::from_ref(&id),
            "a new wording tries again"
        );
        assert_eq!(
            store.unlabelled_searches(V, "other", 10).await.unwrap(),
            std::slice::from_ref(&id),
            "so does a new model"
        );
        // The new era's first failure starts its own count.
        assert_eq!(store.label_failed(&id, V + 1, M, true, 0).await.unwrap(), 1);
        assert_eq!(
            store.unlabelled_searches(V + 1, M, 10).await.unwrap(),
            std::slice::from_ref(&id)
        );
        // Labelled at last, nothing of the failures is kept.
        store
            .put_search_labels(&id, &labels(&n, &[0]), V + 1, M)
            .await
            .unwrap();
        let left = sqlx::query_scalar!("SELECT count(*) FROM search_label_failures")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
