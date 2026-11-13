//! An artifact's own neighbours, by point id: no embedding call; superseded and deprecated skipped.
//! Of any pair the member embedded second finds the other; the sweep backstops missed units.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{Chunk, Provenance};
use crate::store::jobs::Stage;
use crate::store::pairs::PairState;

/// Idle-only: `enqueue` would wind a still-queued unit's attempts back to zero.
pub async fn arm(core: &Core, artifact_id: &str, seq: i64) -> Result<()> {
    core.store
        .rearm_idle_seq(Stage::Relate, "artifact", artifact_id, seq)
        .await
}

pub async fn run(core: &Core, artifact_id: &str) -> Result<()> {
    let me = core.store.get_artifact(artifact_id).await?;
    // Neither a passage nor a merge is anchored (`embed::mark_indexed`), whoever armed the unit. A
    // synthesis stays an anchor.
    if me.provenance == Provenance::Passage || me.provenance == Provenance::Merged {
        return Ok(());
    }
    // Saves the query only: `classify_pair` would skip every pair naming a retired artifact.
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
    // The live generation's threshold, which autonomy moves; read before any await so the lock is
    // never held across one.
    let review_min = core
        .ranking
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .review_min;

    for h in hits {
        // `similarity`, not `score`: `review_min` is a cosine, and `score` is a fused rank
        // elsewhere in this trait.
        let Some(similarity) = h.similarity else {
            tracing::warn!(
                artifact_id,
                neighbour = %h.payload.artifact_id,
                "neighbour came back with no cosine; skipping rather than guessing"
            );
            continue;
        };
        if similarity < review_min {
            continue;
        }
        // Ordinary: the vector store can list a point SQLite has dropped, as a delete lags its
        // reindex.
        let Ok(other) = core.store.get_artifact(&h.payload.artifact_id).await else {
            tracing::debug!(artifact_id = %h.payload.artifact_id, "neighbour is gone");
            continue;
        };
        // One unwritable pair row is no reason to drop the other neighbours.
        match classify_pair(core, &me, &other, similarity).await {
            // `me` is now hidden, and has no duplicates worth recording.
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

/// Containment, not similarity: the only ground on which a pair is hidden without asking.
fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

/// Every rule is local; `auto_supersede` only orders `pairs_to_judge`. Returns whether `a` itself
/// was hidden.
async fn classify_pair(core: &Core, a: &Chunk, b: &Chunk, score: f32) -> Result<bool> {
    if a.id == b.id {
        return Ok(false);
    }
    // Only two live artifacts have a question worth a queue slot, a call or a supersede.
    if [a, b].iter().any(|c| !c.in_results()) {
        return Ok(false);
    }

    // Two rows from one window with a passage on either side are alike for how they were built, and
    // the window job already decided. Two written rows fall through, so containment can catch a
    // call that repeated itself.
    if a.corpus_id.is_some()
        && a.corpus_id == b.corpus_id
        && a.segment_idx.is_some()
        && a.segment_idx == b.segment_idx
        && (a.provenance == Provenance::Passage || b.provenance == Provenance::Passage)
    {
        return Ok(false);
    }

    // Same corpus is the whole condition: two documents sharing a sentence are two sources.
    // `is_some` keeps two merges from matching on `None`.
    if a.corpus_id.is_some() && a.corpus_id == b.corpus_id {
        let (long, short) = if a.text.len() >= b.text.len() {
            (a, b)
        } else {
            (b, a)
        };
        if contains_normalized(&long.text, &short.text) {
            // The row is written first and is also the check: `record_settled_pair` writes only
            // over `pending`, so a settled decision, a person's Restore included, is never
            // re-derived, and a crash never leaves a hide unrecorded.
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
            // The hide did not happen, so the row saying it did must not stand.
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

    // A written same-window pair that containment did not settle merely resembles itself: not
    // filed.
    if a.corpus_id.is_some()
        && a.corpus_id == b.corpus_id
        && a.segment_idx.is_some()
        && a.segment_idx == b.segment_idx
    {
        return Ok(false);
    }

    // Passages are verbatim substrate, alike for how they were cut or because a subject is taught
    // twice. Below the containment block on purpose: only the model question ends here.
    if a.provenance == Provenance::Passage || b.provenance == Provenance::Passage {
        return Ok(false);
    }

    // Alike is not duplicate: on a base teaching one subject, most near pairs are two sources on
    // one topic. The cosine admits nothing alone: containment or one shared document must hold.
    // Nothing is written, not even a link, which records use, not text.
    let (longer, shorter) = if a.text.len() >= b.text.len() {
        (a, b)
    } else {
        (b, a)
    };
    let corroborated = contains_normalized(&longer.text, &shorter.text)
        || (a.corpus_id.is_some() && a.corpus_id == b.corpus_id);
    if !corroborated {
        tracing::debug!(a = %a.id, b = %b.id, score, "alike, but nothing says duplicate");
        return Ok(false);
    }

    core.store.record_pair(&a.id, &b.id, score).await?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::jobs::consolidate::tests::seed;

    #[tokio::test]
    async fn a_merge_armed_by_the_repair_pass_still_files_nothing() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;
        let m = crate::jobs::merge::write(
            &core,
            &crate::infer::prompt::MergedDraft {
                text: "Attach the volume, then mount the filesystem, before writing.".into(),
                title: Some("before writing".into()),
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &ids,
        )
        .await
        .unwrap();

        run(&core, &m.id).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .iter()
                .all(|p| p.a_id != m.id && p.b_id != m.id),
            "the merge was an anchor after all"
        );
    }

    #[tokio::test]
    async fn an_artifact_finds_its_duplicate_the_moment_it_is_embedded() {
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
        // The pair row is unique on (a_id, b_id), whichever side finds it.
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
    async fn the_review_threshold_is_the_live_generation_s_not_the_file_s() {
        // A neighbour at cosine 0.86: below the shipped 0.88, above a rung the base may step down
        // to.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.86, 0.51]),
            ],
        )
        .await;
        assert_eq!(
            core.ranking
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .review_min,
            0.88
        );

        run(&core, &ids[1]).await.unwrap();
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "below the threshold, nothing is asked"
        );

        core.ranking
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .review_min = 0.84;
        run(&core, &ids[1]).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the lowered rung admits it"
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
        // A re-arming or hand-queued job can arrive before the vector exists; failing would burn
        // attempts on a state that resolves itself.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: "never embedded".into(),
                    ..Default::default()
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

    async fn seed_passage(core: &Core, corpus: &str, text: &str) -> String {
        let src = core.store.insert_corpus(corpus, "web", None).await.unwrap();
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    text: text.to_string(),
                    segment_idx: Some(0),
                    ..Default::default()
                }],
                Provenance::Passage,
            )
            .await
            .unwrap()
            .remove(0)
            .id
    }

    #[tokio::test]
    async fn two_passages_from_different_documents_about_one_subject_are_not_a_pair() {
        let core = test_core().await;
        let a_id = seed_passage(&core, "skript-a", "Spuren sind materielle Veraenderungen.").await;
        let b_id = seed_passage(
            &core,
            "skript-b",
            "Als Spur gilt jede materielle Veraenderung.",
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "two passages from two documents are two sources, not a duplicate"
        );
    }

    #[tokio::test]
    async fn a_pair_with_a_passage_on_either_side_is_never_filed() {
        let core = test_core().await;
        let passage =
            seed_passage(&core, "skript-a", "Spuren sind materielle Veraenderungen.").await;
        let captured = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&passage).await.unwrap();
        let b = core.store.get_artifact(&captured).await.unwrap();

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
    async fn cross_corpus_similarity_alone_does_not_reach_the_model() {
        let core = test_core().await;
        let a_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen an Personen oder Sachen.",
            [1.0, 0.0],
        )
        .await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung an Person oder Objekt.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

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
    async fn containment_across_corpora_still_reaches_the_model() {
        let core = test_core().await;
        let a_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen an Personen oder Sachen, \
             die zur Tataufklaerung beitragen koennen.",
            [1.0, 0.0],
        )
        .await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen an Personen oder Sachen,",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

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
    async fn a_refused_pair_does_not_bump_a_link() {
        let core = test_core().await;
        let a_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Spuren sind materielle Veraenderungen.",
            [1.0, 0.0],
        )
        .await;
        let b_id = crate::jobs::consolidate::tests::seed_into_new_corpus(
            &core,
            "Als Spur gilt jede materielle Veraenderung.",
            [0.99, 0.05],
        )
        .await;
        let a = core.store.get_artifact(&a_id).await.unwrap();
        let b = core.store.get_artifact(&b_id).await.unwrap();

        classify_pair(&core, &a, &b, 0.93).await.unwrap();

        assert!(
            core.store.get_link(&a_id, &b_id).await.unwrap().is_none(),
            "a cosine is not evidence that anyone used these together"
        );
    }

    #[tokio::test]
    async fn a_repeated_passage_inside_one_corpus_is_still_superseded_by_containment() {
        // Containment inside one corpus must stay reachable above the passage guard.
        let core = test_core().await;
        let src = core
            .store
            .insert_corpus("skript", "web", None)
            .await
            .unwrap();
        let rows = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[
                    crate::store::artifacts::NewArtifact {
                        text: "Spuren sind materielle Veraenderungen an Personen oder Sachen."
                            .into(),
                        segment_idx: Some(0),
                        ..Default::default()
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "Spuren sind materielle Veraenderungen".into(),
                        segment_idx: Some(1),
                        ..Default::default()
                    },
                ],
                Provenance::Passage,
            )
            .await
            .unwrap();

        classify_pair(&core, &rows[0], &rows[1], 0.97)
            .await
            .unwrap();

        let short = core.store.get_artifact(&rows[1].id).await.unwrap();
        assert!(
            !short.in_results(),
            "a passage wholly inside another in one corpus is still hidden"
        );
    }

    #[tokio::test]
    async fn a_pair_with_no_differing_values_is_queued_not_closed() {
        // A pair stating the same values in different words is the cleanest merge candidate, not
        // "nothing to decide".
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
        // On the stale read the third neighbour would file a pair about an already-hidden artifact.
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
        // Both producers and every sweep re-find the pair; counting each would bring a dismissed
        // pair back.
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

    #[tokio::test]
    async fn two_rows_from_one_window_are_never_a_pair() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str, seg: i64| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            title: Some("same heading".into()),
            segment_idx: Some(seg),
            ..Default::default()
        };
        let written = core
            .store
            .insert_artifacts(&src.id, &[na(0, "mount the disk first", 0)])
            .await
            .unwrap();
        let passage = core
            .store
            .insert_artifacts_with_provenance(
                &src.id,
                &[na(1, "mount the disk first", 0)],
                Provenance::Passage,
            )
            .await
            .unwrap();
        let same: Vec<_> = written.iter().chain(passage.iter()).cloned().collect();
        let other = core
            .store
            .insert_artifacts(&src.id, &[na(2, "mount the disk first", 1)])
            .await
            .unwrap();
        for c in same.iter().chain(other.iter()) {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        // Identical text under one title embeds identically: without the exclusion this is a 1.0
        // pair.
        run(&core, &same[0].id).await.unwrap();
        let pending = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap();
        assert!(
            !pending.iter().any(|p| {
                (p.a_id == same[0].id && p.b_id == same[1].id)
                    || (p.a_id == same[1].id && p.b_id == same[0].id)
            }),
            "same-window pair was filed: {pending:?}"
        );
        let settled = core
            .store
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap();
        assert!(
            pending
                .iter()
                .chain(settled.iter())
                .any(|p| p.a_id == other[0].id || p.b_id == other[0].id),
            "cross-window pair not filed: {pending:?} {settled:?}"
        );
    }

    #[tokio::test]
    async fn one_call_emitting_a_passage_twice_is_still_caught_inside_a_window() {
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            title: Some("same heading".into()),
            segment_idx: Some(0),
            ..Default::default()
        };
        let rows = core
            .store
            .insert_artifacts(
                &src.id,
                &[
                    na(0, "mount the disk first, then run the installer"),
                    na(1, "mount the disk first"),
                ],
            )
            .await
            .unwrap();
        for c in &rows {
            crate::jobs::embed::run(&core, &c.id).await.unwrap();
        }
        let (long, short) = (&rows[0], &rows[1]);
        let a = core.store.get_artifact(&long.id).await.unwrap();
        let b = core.store.get_artifact(&short.id).await.unwrap();
        classify_pair(&core, &a, &b, 0.95).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .iter()
                .any(|p| (p.a_id == a.id && p.b_id == b.id) || (p.a_id == b.id && p.b_id == a.id)),
            "a duplicate inside one window was not settled"
        );
        let after = core.store.get_artifact(&short.id).await.unwrap();
        assert!(!after.in_results(), "the duplicated copy is still visible");
    }

    #[tokio::test]
    async fn a_pair_above_auto_supersede_is_filed_for_the_judge_not_hidden() {
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the first wording", [1.0, 0.0]),
                ("the second wording", [1.0, 0.0]),
            ],
        )
        .await;
        run(&core, &ids[1]).await.unwrap();
        assert!(core.store.get_artifact(&ids[0]).await.unwrap().in_results());
        assert!(core.store.get_artifact(&ids[1]).await.unwrap().in_results());
        let to_judge = core.store.pairs_to_judge(10).await.unwrap();
        assert_eq!(to_judge.len(), 1, "{to_judge:?}");
        assert!(
            core.store
                .pairs_by_state(PairState::NearIdentical, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
