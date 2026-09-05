//! Writing the evaluation corpus straight out of the live database.
//!
//! The artifacts have already been synthesised, so this costs no completions,
//! and it keeps their production ids — which means re-exporting does not
//! invalidate the pairs. The queries come from searches actually made, and the
//! expectations from verdicts actually given.

use crate::eval::{
    EvalPair, EvalQuestion, FrozenArtifact, save_artifacts, save_pairs, save_questions,
};
use crate::store::Store;
use anyhow::Result;
use sqlx::Row;
use std::path::Path;

/// Returns how many artifacts, pairs and questions were written.
pub async fn export(store: &Store, dir: &Path) -> Result<(usize, usize, usize)> {
    let artifacts = store.all_active_artifacts().await?;
    let known: std::collections::HashSet<String> = artifacts.iter().map(|c| c.id.clone()).collect();

    let frozen: Vec<FrozenArtifact> = artifacts
        .iter()
        .map(|c| FrozenArtifact {
            id: c.id.clone(),
            // Empty for a merged artifact, which has no single source document.
            // The frozen set records what an artifact *is*, and a merge's
            // sources are its lineage rather than a corpus id.
            source: c.corpus_id.clone().unwrap_or_default(),
            text: c.text.clone(),
            title: c.title.clone(),
            category: c.category.clone(),
            tags: c.tags.clone(),
        })
        .collect();

    let rows = sqlx::query(
        "SELECT query, expect_id, door, judged_at FROM search_events
         WHERE verdict = 'hit' AND expect_id IS NOT NULL
         ORDER BY judged_at",
    )
    .fetch_all(&store.pool)
    .await?;

    let mut pairs = Vec::new();
    let mut dropped = 0usize;
    for r in &rows {
        let expect: String = r.get("expect_id");
        // A pair naming an artifact that has since been deleted would be scored
        // as a miss forever, and read as a ranking problem rather than a gone
        // one.
        if !known.contains(&expect) {
            dropped += 1;
            continue;
        }
        pairs.push(EvalPair {
            query: r.get("query"),
            expect,
            note: Some(format!(
                "{} · judged {}",
                r.get::<String, _>("door"),
                r.get::<i64, _>("judged_at")
            )),
        });
    }
    if dropped > 0 {
        tracing::warn!(dropped, "pairs skipped: their artifact no longer exists");
    }

    // Judged questions, with the artifacts the operator said carried the
    // answer. A carrier whose artifact is gone is dropped like a stale pair;
    // the question stays, because it still says whether the base held it.
    let asks = sqlx::query(
        "SELECT id, question, verdict, judged_at FROM ask_events
         WHERE verdict IS NOT NULL ORDER BY judged_at",
    )
    .fetch_all(&store.pool)
    .await?;
    let mut questions = Vec::with_capacity(asks.len());
    let mut lost_carriers = 0usize;
    for r in &asks {
        let id: String = r.get("id");
        let verdict: String = r.get("verdict");
        let carriers: Vec<String> = sqlx::query_scalar(
            "SELECT artifact_id FROM ask_citations WHERE event_id = ? AND carried = 1 ORDER BY n",
        )
        .bind(&id)
        .fetch_all(&store.pool)
        .await?;
        let (kept, lost): (Vec<String>, Vec<String>) =
            carriers.into_iter().partition(|c| known.contains(c));
        lost_carriers += lost.len();
        // The invariant `EvalQuestion` documents, enforced at the one place
        // that writes the file. `toggle_carried` deliberately does not overrule
        // a verdict already given, so an operator who judges `wrong` and then
        // marks what the answer leaned on leaves a carrier behind a verdict
        // that says the answer was not right — and a carrier under `wrong` is
        // not a statement that the artifact should have been cited, which is
        // the only thing `expect` means.
        let expect = match verdict.as_str() {
            "right" => kept,
            _ => Vec::new(),
        };
        questions.push(EvalQuestion {
            question: r.get("question"),
            verdict,
            expect,
            note: Some(format!("judged {}", r.get::<i64, _>("judged_at"))),
        });
    }
    if lost_carriers > 0 {
        tracing::warn!(
            lost_carriers,
            "carriers skipped: their artifact no longer exists"
        );
    }

    save_artifacts(dir, &frozen)?;
    save_pairs(dir, &pairs)?;
    save_questions(dir, &questions)?;
    Ok((frozen.len(), pairs.len(), questions.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::NewArtifact;
    use crate::store::feedback::{Door, NewCandidate, NewEvent, Verdict};

    async fn seed_one_artifact(store: &Store) -> String {
        seed_titled(store, "raw text for export").await
    }

    /// Same `title_hint`, different text — two captures of what the operator
    /// thinks of as the same document. `content_hash` is unique, so the raw
    /// text has to differ for them to be two corpora at all.
    async fn seed_titled(store: &Store, raw: &str) -> String {
        let src = store
            .insert_corpus(raw, "test", Some("fat.txt"))
            .await
            .unwrap();
        let made = store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: "a deleted entry keeps its name but loses its start cluster".into(),
                    corpus_span: None,
                    title: Some("deleted entries".into()),
                    category: Some("concept".into()),
                    tags: vec!["fat".into()],
                    segment_idx: None,
                    caveats: vec![],
                }],
            )
            .await
            .unwrap();
        made[0].id.clone()
    }

    async fn record_one_search(store: &Store, query: &str, expect: &str) -> String {
        store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![NewCandidate {
                        artifact_id: expect.into(),
                        score: 0.9,
                        similarity: Some(0.7),
                        shown: true,
                        band: false,
                    }],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_judged_hit_becomes_a_pair_and_its_artifact_is_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let artifact_id = seed_one_artifact(&store).await;
        let event = record_one_search(&store, "how do I read a deleted entry", &artifact_id).await;
        store
            .judge_hit(&event, &artifact_id, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();

        let (artifacts, pairs, _) = export(&store, dir.path()).await.unwrap();
        assert_eq!((artifacts, pairs), (1, 1));

        let frozen = crate::eval::load_artifacts(dir.path()).unwrap();
        assert_eq!(
            frozen[0].id, artifact_id,
            "ids must stay the production ones, or a re-export invalidates every pair"
        );
        let corpus = store
            .get_artifact(&artifact_id)
            .await
            .unwrap()
            .corpus_id
            .expect("a captured artifact names its corpus");
        assert_eq!(
            frozen[0].source, corpus,
            "the cap groups by this, so it has to name a corpus uniquely"
        );

        let loaded = crate::eval::load_pairs(dir.path()).unwrap();
        assert_eq!(loaded[0].expect, artifact_id);
        assert_eq!(loaded[0].query, "how do I read a deleted entry");
    }

    #[tokio::test]
    async fn two_corpora_sharing_a_title_stay_two_sources() {
        // `source` is what the per-corpus cap groups by when the harness
        // rebuilds the base, so it has to identify a corpus and not merely
        // describe one. Two pasted documents both called `fat.txt` merged into
        // one source, and the cap that lets each corpus contribute three
        // results started applying across both — the harness then measured a
        // narrower base than the search page actually runs.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let first = seed_titled(&store, "one capture of the manual").await;
        let second = seed_titled(&store, "a later capture of the same manual").await;
        assert_ne!(first, second);

        export(&store, dir.path()).await.unwrap();
        let frozen = crate::eval::load_artifacts(dir.path()).unwrap();
        assert_eq!(frozen.len(), 2);
        assert_ne!(
            frozen[0].source, frozen[1].source,
            "artifacts from different corpora collapsed into one source"
        );
    }

    #[tokio::test]
    async fn a_pair_pointing_at_a_deleted_artifact_is_left_out() {
        // Scored as a miss it would look like a ranking problem forever.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let event = record_one_search(&store, "gone", "no-such-artifact").await;
        store
            .judge_hit(
                &event,
                "no-such-artifact",
                crate::store::feedback::Labeller::Deck,
            )
            .await
            .unwrap();

        let (_, pairs, _) = export(&store, dir.path()).await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn gaps_and_discards_never_become_pairs() {
        // One has no answer to name, the other was not a question.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let g = record_one_search(&store, "nothing about this", "x").await;
        store
            .judge(&g, Verdict::Gap, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();
        let d = record_one_search(&store, "asdf", "x").await;
        store
            .judge(&d, Verdict::Discard, crate::store::feedback::Labeller::Deck)
            .await
            .unwrap();

        let (_, pairs, _) = export(&store, dir.path()).await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn a_superseded_artifact_stays_out_of_the_frozen_corpus() {
        // The benchmark has to see the base the search page sees, or it scores
        // a program nobody runs.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let id = seed_one_artifact(&store).await;
        sqlx::query("UPDATE artifacts SET status = 'deprecated' WHERE id = ?")
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();

        let (artifacts, _, _) = export(&store, dir.path()).await.unwrap();
        assert_eq!(artifacts, 0);
    }

    async fn record_ask(store: &Store, q: &str, cited: &[&str]) -> String {
        store
            .record_ask(crate::store::asks::NewAsk {
                question: q.into(),
                scope: None,
                filters: "{}".into(),
                query_vec: vec![0.0; 4],
                embed_model: "fake".into(),
                answer: "a".into(),
                abstained: false,
                dropped: 0,
                truncated: false,
                unsupported: 0,
                citations: cited
                    .iter()
                    .map(|id| crate::store::asks::NewAskCitation {
                        artifact_id: id.to_string(),
                        score: 1.0,
                        // The harness names what the answer drew on, not what
                        // it was shown; here they are the same list.
                        used: true,
                    })
                    .collect(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_judged_question_becomes_an_eval_question_with_its_carriers() {
        let store = Store::memory().await.unwrap();
        let art = seed_one_artifact(&store).await;
        let id = record_ask(&store, "how do I", &[&art]).await;
        store.toggle_carried(&id, 1).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, _, questions) = export(&store, dir.path()).await.unwrap();
        assert_eq!(questions, 1);
        let qs = crate::eval::load_questions(dir.path()).unwrap();
        assert_eq!(qs[0].question, "how do I");
        assert_eq!(qs[0].verdict, "right");
        assert_eq!(qs[0].expect, vec![art]);
    }

    #[tokio::test]
    async fn unjudged_questions_are_not_exported_and_gone_carriers_are_dropped() {
        let store = Store::memory().await.unwrap();
        let art = seed_one_artifact(&store).await;
        record_ask(&store, "unjudged", &[&art]).await;
        let id = record_ask(&store, "carrier gone", &[&art, "deleted-artifact"]).await;
        store.toggle_carried(&id, 1).await.unwrap();
        store.toggle_carried(&id, 2).await.unwrap();
        let nothing = record_ask(&store, "not here", &[]).await;
        store
            .judge_ask(&nothing, crate::store::asks::AskVerdict::NothingHere)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_, _, n) = export(&store, dir.path()).await.unwrap();
        assert_eq!(n, 2);
        let qs = crate::eval::load_questions(dir.path()).unwrap();
        let gone = qs.iter().find(|q| q.question == "carrier gone").unwrap();
        assert_eq!(gone.expect, vec![art]);
        let none = qs.iter().find(|q| q.question == "not here").unwrap();
        assert_eq!(none.verdict, "nothing_here");
        assert!(none.expect.is_empty());
    }

    #[tokio::test]
    async fn a_carrier_behind_a_wrong_verdict_is_not_exported_as_an_expectation() {
        // `toggle_carried` does not overrule a verdict already given, so this
        // order — judge wrong, then mark what the answer leaned on — is
        // reachable from the page. A carrier under `wrong` says the answer used
        // that artifact, not that it should have; exporting it as `expect`
        // would score citation recall against an answer nobody stands behind.
        let store = Store::memory().await.unwrap();
        let art = seed_one_artifact(&store).await;
        let id = record_ask(&store, "wrong but cited", &[&art]).await;
        store
            .judge_ask(&id, crate::store::asks::AskVerdict::Wrong)
            .await
            .unwrap();
        store.toggle_carried(&id, 1).await.unwrap();
        assert_eq!(
            store.ask_event(&id).await.unwrap().unwrap().verdict,
            Some(crate::store::asks::AskVerdict::Wrong),
            "marking a carrier must not have promoted the verdict"
        );

        let dir = tempfile::tempdir().unwrap();
        export(&store, dir.path()).await.unwrap();
        let qs = crate::eval::load_questions(dir.path()).unwrap();
        assert_eq!(qs[0].verdict, "wrong");
        assert!(qs[0].expect.is_empty(), "{:?}", qs[0].expect);
    }
}
