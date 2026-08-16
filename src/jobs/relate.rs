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
use crate::store::artifacts::Chunk;
use crate::store::jobs::Stage;
use crate::store::pairs::PairState;

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
        match classify_pair(core, &me, &other, similarity).await {
            // `me` was the contained side and is now hidden. It is read once,
            // before the loop, so every later neighbour would be judged against
            // a status that is no longer true — `Core::supersede` re-reads both
            // sides and refuses the second attempt, so nothing is corrupted, but
            // each remaining neighbour costs a round trip and a warning about an
            // ordinary outcome. A hidden artifact has no duplicates worth
            // recording; that is the same rule the top of this function applies.
            Ok(true) => {
                tracing::debug!(artifact_id, "hidden as a duplicate; leaving its neighbours");
                break;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(a = %me.id, b = %other.id, error = %e, "could not classify a neighbour")
            }
        }
    }
    Ok(())
}

/// Is the whole of one artifact inside the other, whitespace aside?
///
/// Not a similarity — containment. A score says two texts are alike; this says
/// one of them adds nothing, which is the only ground on which a pair below
/// `auto_supersede` is hidden without asking anyone.
fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

/// Turn one scored pair into one decision. Nothing here calls a model: every
/// rule is local, and the two that settle a pair outright — containment, and
/// the `auto_supersede` band — are why most near pairs cost nothing at all.
///
/// Returns whether `a` itself was hidden by the decision, which is the caller's
/// signal that the artifact it is scanning neighbours for is no longer live.
async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<bool> {
    if a.id == b.id {
        return Ok(false);
    }
    // Only two live artifacts have a question worth a queue slot, a model call,
    // or a supersede. A retired artifact must not win against a live one, and a
    // pair that is already resolved has nothing left to decide.
    if [a, b].iter().any(|c| !c.in_results()) {
        return Ok(false);
    }

    // The free band. Filed, not acted on: resolving pairs one at a time leaves A
    // pointing at a B that is itself hidden, and following that chain is
    // something no page and no reader can do. The sweep's union-find groups
    // these rows first and then picks one survivor per cluster.
    if score >= core.consolidate.auto_supersede {
        core.store
            .record_settled_pair(&a.id, &b.id, score, PairState::NearIdentical)
            .await?;
        return Ok(false);
    }

    // One synthesis call emitting the same passage twice: the shorter text is
    // wholly inside the longer, and both came out of the same document. That is
    // a defect in one artifact rather than two sources saying different things,
    // and nothing is lost by hiding it — the survivor says everything it said,
    // Ops lists it, and one press undoes it.
    //
    // Same corpus is the whole of the condition. Two documents that share a
    // sentence are two sources, and hiding one of those on a 0.9 similarity is
    // exactly what `auto_supersede` refuses to do. A merged artifact has no
    // corpus, so `is_some` also keeps two merges from matching on `None == None`.
    if a.corpus_id.is_some() && a.corpus_id == b.corpus_id {
        let (long, short) = if a.text.len() >= b.text.len() {
            (a, b)
        } else {
            (b, a)
        };
        if contains_normalized(&long.text, &short.text) {
            // The row first, and the row is also the check. The rule is
            // deterministic — these two texts satisfy it every time either
            // artifact is related again — so a row that is already settled
            // carries a decision this rule must not re-derive, most importantly
            // a person's Restore after an earlier hide.
            //
            // `record_settled_pair` only writes over `pending`, so the write
            // answers that question itself: `false` means a settled row is
            // already there and this pair is not ours to act on. Asking first
            // and writing after the hide left two windows, and the second one
            // mattered — a crash between the hide and the row left the artifact
            // hidden with nothing recording why. It was then invisible to the
            // `in_results` guard above, so nothing re-derived it, and invisible
            // to this check, so the next relate unit after an operator pressed
            // Restore hid it again: exactly the failure this row exists to
            // prevent. Written first, a crash in the same window leaves a
            // settled row over a duplicate that is still visible, which costs a
            // listing and hides nothing.
            if !core
                .store
                .record_settled_pair(&a.id, &b.id, score, PairState::NoConflict)
                .await?
            {
                tracing::debug!(
                    a = %a.id,
                    b = %b.id,
                    "a duplicated passage is already settled; leaving it"
                );
                return Ok(false);
            }
            if crate::jobs::try_supersede(
                core,
                &short.id,
                &long.id,
                "a passage one synthesis call emitted twice",
            )
            .await
            {
                return Ok(short.id == a.id);
            }
            // The hide did not happen, so the row saying it did must not stand:
            // it would leave this pair settled over two artifacts that are both
            // still visible, and nothing would ever look at it again.
            if let Err(e) = core
                .store
                .unsettle_pair(&a.id, &b.id, PairState::NoConflict)
                .await
            {
                tracing::warn!(
                    a = %a.id,
                    b = %b.id,
                    error = %e,
                    "could not reopen a pair whose supersede failed; it stays settled"
                );
            }
            return Ok(false);
        }
    }

    // Everything else is a question for the dedupe pass.
    //
    // `may_disagree` used to gate this, filing a pair with no differing values
    // as `NoConflict` and leaving both artifacts in every result set. That was
    // right for the question it was written for — "do these two contradict?" —
    // and is backwards for deduplication: a pair stating the same values in
    // different words is the *cleanest* thing there is to merge, and the old
    // rule made it the one case the model was never shown.
    //
    // It survives as a prior in the prompt and as the input to the merge
    // verification. It is no longer an admission gate. See the design, §6.5.
    core.store.record_pair(&a.id, &b.id, score).await?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::jobs::consolidate::tests::seed;

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
    async fn pair_of(core: &Core, ids: &[String]) -> (Chunk, Chunk) {
        (
            core.store.get_artifact(&ids[0]).await.unwrap(),
            core.store.get_artifact(&ids[1]).await.unwrap(),
        )
    }

    #[tokio::test]
    async fn a_pair_with_no_differing_values_is_queued_not_closed() {
        // The polarity change. `may_disagree` admits a pair only when both
        // sides state values AND those values differ, which is backwards for
        // deduplication: the pairs it discarded are the cleanest merge
        // candidates. Two artifacts at 0.93 saying the same thing in different
        // words used to be filed "nothing to decide", and both stayed in every
        // result set forever.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let (a, b) = pair_of(&core, &ids).await;

        classify_pair(&core, &a, &b, 0.93).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_pair_at_auto_supersede_is_filed_for_the_cluster_pass() {
        // It must not reach the dedupe queue: that band is answered by a rule
        // that costs nothing. It must also not be superseded here — pairwise
        // resolution is what `Clusters` exists to avoid, because A loses to B
        // and B then loses to C, leaving A pointing at something hidden.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let (a, b) = pair_of(&core, &ids).await;

        classify_pair(&core, &a, &b, 0.999).await.unwrap();
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "the pair was resolved pairwise instead of being filed"
            );
        }
        assert!(core.store.pairs_to_judge(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pair_naming_a_hidden_artifact_is_skipped() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();
        let (a, b) = pair_of(&core, &ids).await;

        classify_pair(&core, &a, &b, 0.93).await.unwrap();
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_artifact_hidden_by_its_first_neighbour_stops_there() {
        // `me` is read once, before the loop. The first neighbour that contains
        // it hides it, and every neighbour after that would be judged against a
        // status that is no longer true: `Core::supersede` re-reads both sides
        // and refuses, so the row is never wrong, but the attempt costs a round
        // trip and logs a warning about an entirely ordinary outcome.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem.", [1.0, 0.0]),
                ("Mount the filesystem. Then write.", [0.94, 0.34]),
                ("Attach the volume before writing.", [0.92, 0.39]),
            ],
        )
        .await;

        run(&core, &ids[0]).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[1].as_str()),
            "the contained artifact should have been hidden by its nearest neighbour"
        );
        // The third neighbour is near but contains nothing, so on the stale
        // read it reaches `record_pair` and files a question about an artifact
        // that is already hidden — a pair no one will ever be asked to settle.
        assert!(
            core.store
                .pair_state_between(&ids[0], &ids[2])
                .await
                .unwrap()
                .is_none(),
            "the scan should have stopped at the neighbour that hid it"
        );
    }

    #[tokio::test]
    async fn recording_the_same_pair_twice_changes_nothing() {
        // Both producers find the same pair, and the sweep re-finds it on every
        // run. If the second recording counted, the tallies would grow forever
        // and a dismissed pair would come back.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let (a, b) = pair_of(&core, &ids).await;

        classify_pair(&core, &a, &b, 0.93).await.unwrap();
        classify_pair(&core, &b, &a, 0.93).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
