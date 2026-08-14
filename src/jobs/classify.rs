//! Turning one scored pair into one decision.
//!
//! Two things discover near pairs now — the per-artifact `Relate` unit, which
//! is exact, and the sweep's sampled scan, which is the backlog and the
//! backstop. Both must reach the same conclusion about the same pair. Leaving
//! these rules in the sweep's body meant the second producer would have arrived
//! with a copy, and a copy diverges silently: you find out when the outcome
//! starts depending on which path saw a pair first.
//!
//! Nothing here calls a model. Every rule is local, and the two that settle a
//! pair outright — containment, and the `auto_supersede` band — are why most
//! near pairs still cost nothing at all.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::pairs::PairState;

/// What was decided about a pair, for the caller's tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing was written: a side is not active, or the two are one artifact.
    Skipped,
    /// Filed at or above `auto_supersede`, for the sweep's clustering pass.
    NearIdentical,
    /// One text was wholly inside the other and both came from one corpus. The
    /// shorter is superseded here, with no call and no queue slot.
    Contained,
    /// Filed as `Pending`. A dedupe unit will decide.
    Queued,
    /// Already recorded, or already answered. Nothing changed.
    Unchanged,
}

/// Is the whole of one artifact inside the other, whitespace aside?
///
/// Not a similarity — containment. A score says two texts are alike; this says
/// one of them adds nothing, which is the only ground on which a pair below
/// `auto_supersede` is hidden without asking anyone.
pub fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

pub async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<Verdict> {
    if a.id == b.id {
        return Ok(Verdict::Skipped);
    }
    // Only two live artifacts have a question worth a queue slot, a model call,
    // or a supersede. A retired artifact must not win against a live one, and a
    // pair that is already resolved has nothing left to decide.
    if [a, b]
        .iter()
        .any(|c| c.status != ArtifactStatus::Active || c.superseded_by.is_some())
    {
        return Ok(Verdict::Skipped);
    }

    // The free band. Filed, not acted on: resolving pairs one at a time leaves A
    // pointing at a B that is itself hidden, and following that chain is
    // something no page and no reader can do. The sweep's union-find groups
    // these rows first and then picks one survivor per cluster.
    if score >= core.consolidate.auto_supersede {
        let changed = core
            .store
            .record_settled_pair(&a.id, &b.id, score, PairState::NearIdentical)
            .await?;
        return Ok(if changed {
            Verdict::NearIdentical
        } else {
            Verdict::Unchanged
        });
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
            // Warn and carry on. `supersede` refuses a side that is no longer
            // active, and these statuses were read a moment ago, so an operator
            // deprecating one in between is an ordinary race rather than a
            // reason to fail the caller's whole sweep.
            if let Err(e) = core.supersede(&short.id, &long.id).await {
                tracing::warn!(
                    superseded = %short.id,
                    by = %long.id,
                    error = %e,
                    "could not hide a duplicated passage; it stays active"
                );
                return Ok(Verdict::Skipped);
            }
            tracing::info!(
                superseded = %short.id,
                by = %long.id,
                "hid a passage one synthesis call emitted twice"
            );
            return Ok(Verdict::Contained);
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
    let changed = core.store.record_pair(&a.id, &b.id, score).await?;
    Ok(if changed {
        Verdict::Queued
    } else {
        Verdict::Unchanged
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::jobs::consolidate::tests::{seed, seed_into_new_corpus};
    use crate::store::pairs::PairState;

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

        assert_eq!(
            classify_pair(&core, &a, &b, 0.93).await.unwrap(),
            Verdict::Queued
        );
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

        assert_eq!(
            classify_pair(&core, &a, &b, 0.999).await.unwrap(),
            Verdict::NearIdentical
        );
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
    async fn one_synthesis_call_emitting_a_passage_twice_resolves_itself() {
        // Same corpus, one text wholly inside the other: a defect in one
        // artifact rather than two sources saying different things. Nothing is
        // lost by hiding it, and it costs no call.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                (
                    "Bind mounts attach a directory elsewhere. Use mount --bind for it.",
                    [1.0, 0.0],
                ),
                ("Bind mounts attach a directory elsewhere.", [0.93, 0.37]),
            ],
        )
        .await;
        let (a, b) = pair_of(&core, &ids).await;

        assert_eq!(
            classify_pair(&core, &a, &b, 0.93).await.unwrap(),
            Verdict::Contained
        );
        assert_eq!(
            core.store
                .get_artifact(&ids[1])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[0].as_str())
        );
    }

    #[tokio::test]
    async fn containment_across_two_corpora_is_left_alone() {
        // Two documents that happen to share a sentence are two sources, and
        // this is exactly the case auto_supersede refuses to act on below 0.95.
        let core = test_core().await;
        let a_ids = seed(
            &core,
            &[(
                "Bind mounts attach a directory elsewhere. Use mount --bind for it.",
                [1.0, 0.0],
            )],
        )
        .await;
        let b_id = seed_into_new_corpus(
            &core,
            "Bind mounts attach a directory elsewhere.",
            [0.93, 0.37],
        )
        .await;
        let a = core.store.get_artifact(&a_ids[0]).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        assert_eq!(
            classify_pair(&core, &a, &b, 0.93).await.unwrap(),
            Verdict::Queued
        );
        for id in [&a_ids[0], &b_id] {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "two documents sharing a sentence are two sources"
            );
        }
    }

    #[tokio::test]
    async fn a_pair_naming_a_hidden_artifact_is_skipped() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();
        let (a, b) = pair_of(&core, &ids).await;

        assert_eq!(
            classify_pair(&core, &a, &b, 0.93).await.unwrap(),
            Verdict::Skipped
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
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

        assert_eq!(
            classify_pair(&core, &a, &b, 0.93).await.unwrap(),
            Verdict::Queued
        );
        assert_eq!(
            classify_pair(&core, &b, &a, 0.93).await.unwrap(),
            Verdict::Unchanged,
            "the reversed pair was recorded a second time"
        );
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
