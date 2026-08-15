//! What else is this artifact already saying?
//!
//! One artifact, its own neighbours, one query. `VectorStore::neighbours`
//! addresses the point by id, so the vector is looked up in the index and no
//! embedding call is paid, and it already excludes superseded and deprecated
//! points. Coverage is 1, independent of N, at one round trip per artifact.
//!
//! Completeness argument: for a pair (X, Y), the member embedded **second**
//! finds the other. When X's unit runs, Y is either already indexed — X finds
//! it — or it is not, and Y finds X when its own unit runs. Embedded in the
//! same batch, both units run after the shared upsert and both see each other.
//! No pair falls through, provided both units run; the sweep remains as the
//! backstop for the ones that did not.
//!
//! A separate unit rather than a tail on `embed_batch`: a failing Qdrant query
//! would otherwise fail the embed job, whose retry pays for the embedding all
//! over again. Two failure domains, two units — the same reasoning that split
//! the judge calls out of the sweep.

use crate::core::Core;
use crate::error::Result;
use crate::store::jobs::Stage;

/// Queue a neighbour query for this artifact.
///
/// Idle-only, like every other automatic arming: a re-embed can reach this
/// while an earlier unit is still queued, and `enqueue` would wind that unit's
/// attempts back to zero underneath it.
pub async fn arm(core: &Core, artifact_id: &str, seq: i64) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Relate, "artifact", artifact_id, seq)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let me = core.store.get_artifact(artifact_id).await?;
    // A retired artifact has no duplicates worth recording. Every pair naming
    // it would be skipped by `classify_pair` anyway, so this saves the query
    // rather than changing the outcome.
    if !me.in_results() {
        tracing::debug!(
            artifact_id,
            status = me.status.as_str(),
            "skipping a hidden artifact"
        );
        return Ok(());
    }

    let hits = core
        .vectors
        .neighbours(artifact_id, core.consolidate.per_point)
        .await?;

    for h in hits {
        // `similarity` and not `score`. `review_min` is a cosine, and `score`
        // is a fused rank everywhere else in this trait — `neighbours` fills
        // the cosine in precisely so this comparison means what it reads as.
        let Some(similarity) = h.similarity else {
            tracing::warn!(
                artifact_id,
                neighbour = %h.payload.artifact_id,
                "neighbour came back with no cosine; skipping rather than guessing"
            );
            continue;
        };
        if similarity < core.consolidate.review_min {
            continue;
        }
        // Ordinary rather than an error: the vector store can list a point
        // SQLite has already dropped, because a delete lags its reindex.
        let Ok(other) = core.store.get_artifact(&h.payload.artifact_id).await else {
            tracing::debug!(artifact_id = %h.payload.artifact_id, "neighbour is gone");
            continue;
        };
        // Warn and carry on, as the sweep does. One unwritable pair row is no
        // reason to drop the other neighbours, and the next run finds it again.
        if let Err(e) = crate::jobs::classify::classify_pair(core, &me, &other, similarity).await {
            tracing::warn!(a = %me.id, b = %other.id, error = %e, "could not classify a neighbour");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::jobs::consolidate::tests::seed;
    use crate::store::pairs::PairState;

    #[tokio::test]
    async fn an_artifact_finds_its_duplicate_the_moment_it_is_embedded() {
        // Asking one artifact for its own neighbours costs one Qdrant query,
        // no embedding call, and is exact.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        run(&core, &ids[1]).await.unwrap();

        let pending = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "the unit found no duplicate");
        assert!(
            [&pending[0].a_id, &pending[0].b_id].contains(&&ids[0])
                && [&pending[0].a_id, &pending[0].b_id].contains(&&ids[1])
        );
    }

    #[tokio::test]
    async fn a_pair_is_found_by_whichever_member_is_embedded_second() {
        // The completeness argument, and the reason the sweep can stop being
        // the primary detector. Running both units must not double-file: the
        // pair row is unique on (a_id, b_id) whichever way round it is seen.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        run(&core, &ids[0]).await.unwrap();
        let from_a = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .len();
        run(&core, &ids[1]).await.unwrap();
        let after_b = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .len();

        assert_eq!(from_a, 1, "the first member found nothing");
        assert_eq!(after_b, 1, "the second member filed the same pair twice");
    }

    #[tokio::test]
    async fn a_near_identical_neighbour_never_costs_a_model_call() {
        // At or above `auto_supersede` the pair is settled for free by
        // clustering. Filing it as an ordinary pending pair would arm a dedupe
        // unit and spend a call on the one case where the cheap rule is already
        // right — the free path quietly becoming a paid one.
        let core = test_core().await;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let ids = core
            .store
            .all_active_artifacts()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect::<Vec<_>>();

        run(&core, &ids[1]).await.unwrap();

        assert!(core.store.pairs_to_judge(10).await.unwrap().is_empty());
        assert_eq!(
            core.store
                .pairs_by_state(PairState::NearIdentical, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unrelated_neighbour_is_left_entirely_alone() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;

        run(&core, &ids[1]).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_retired_artifact_asks_for_nothing() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        core.deprecate(&ids[1]).await.unwrap();

        run(&core, &ids[1]).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "a retired artifact filed a pair about itself"
        );
    }

    #[tokio::test]
    async fn an_artifact_that_is_not_indexed_yet_is_not_an_error() {
        // `neighbours` answers an unknown point with an empty list rather than
        // failing, and this unit is armed from the embed path — but a
        // re-arming, a restore, or a hand-queued job can reach it before the
        // vector exists. Failing here would burn the unit's attempts on a state
        // that resolves itself.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: "never embedded".into(),
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

        run(&core, &made[0].id).await.unwrap();
    }
}
