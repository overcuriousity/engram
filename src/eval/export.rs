//! Writing the evaluation corpus straight out of the live database.
//!
//! Cheaper and steadier than `eval-prepare`: the artifacts have already been
//! synthesised, so this costs no completions, and it keeps their production
//! ids — which means re-exporting does not invalidate the pairs the way
//! re-freezing does. The queries come from searches actually made, and the
//! expectations from verdicts actually given.

use crate::eval::{EvalPair, FrozenArtifact, save_artifacts, save_pairs};
use crate::store::Store;
use anyhow::Result;
use sqlx::Row;
use std::path::Path;

/// Returns how many artifacts and how many pairs were written.
pub async fn export(store: &Store, dir: &Path) -> Result<(usize, usize)> {
    let artifacts = store.all_active_artifacts().await?;
    let known: std::collections::HashSet<String> = artifacts.iter().map(|c| c.id.clone()).collect();

    let frozen: Vec<FrozenArtifact> = artifacts
        .iter()
        .map(|c| FrozenArtifact {
            id: c.id.clone(),
            source: c.corpus_id.clone(),
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

    save_artifacts(dir, &frozen)?;
    save_pairs(dir, &pairs)?;
    Ok((frozen.len(), pairs.len()))
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
                    }],
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
        store.judge_hit(&event, &artifact_id).await.unwrap();

        let (artifacts, pairs) = export(&store, dir.path()).await.unwrap();
        assert_eq!((artifacts, pairs), (1, 1));

        let frozen = crate::eval::load_artifacts(dir.path()).unwrap();
        assert_eq!(
            frozen[0].id, artifact_id,
            "ids must stay the production ones, or a re-export invalidates every pair"
        );
        let corpus = store.get_artifact(&artifact_id).await.unwrap().corpus_id;
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
        store.judge_hit(&event, "no-such-artifact").await.unwrap();

        let (_, pairs) = export(&store, dir.path()).await.unwrap();
        assert_eq!(pairs, 0);
    }

    #[tokio::test]
    async fn gaps_and_discards_never_become_pairs() {
        // One has no answer to name, the other was not a question.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().await.unwrap();
        let g = record_one_search(&store, "nothing about this", "x").await;
        store.judge(&g, Verdict::Gap).await.unwrap();
        let d = record_one_search(&store, "asdf", "x").await;
        store.judge(&d, Verdict::Discard).await.unwrap();

        let (_, pairs) = export(&store, dir.path()).await.unwrap();
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

        let (artifacts, _) = export(&store, dir.path()).await.unwrap();
        assert_eq!(artifacts, 0);
    }
}
