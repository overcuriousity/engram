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
use crate::store::artifacts::{Chunk, Provenance};
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
    // Neither a passage nor a merge is anchored: see `embed::mark_indexed` for
    // why each is withheld. Said here too, so a unit armed some other way —
    // the sweep's repair pass, an operator, a re-embed — files nothing.
    //
    // `Merged` and not `is_model_written()`, the same distinction the embed
    // path draws: a synthesis is ordinary dedupe material and stays an anchor.
    if me.provenance == Provenance::Passage || me.provenance == Provenance::Merged {
        return Ok(());
    }
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
/// one of them adds nothing, which is now the only ground on which any pair is
/// hidden without asking anyone: the `auto_supersede` band settles nothing on
/// the score alone any more (see `classify_pair`).
fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

/// Turn one scored pair into one decision. Nothing here calls a model: every
/// rule is local, and the ones that settle a pair outright — an exhausted
/// side, two rows from one window, containment — are why most near pairs cost
/// nothing at all. `auto_supersede` is not among them: it orders
/// `pairs_to_judge` and no longer hides anything by itself.
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

    // Two rows from one window are not a pair. Neighbours under one heading
    // are similar for how they were built, not for what they say; and a
    // promoted artifact beside the passage it left standing is the window
    // job's decision — the majority rule — already made. Sending that pair to
    // the judge would spend a call to merge, and so hide behind model text,
    // exactly the verbatim passage promotion just decided to keep.
    //
    // Both of those cases have a passage on at least one side, which is what
    // the last clause asks. Two *written* rows from one window are the case
    // neither reason covers: one synthesis call emitting the same passage
    // twice is a defect in its output, not a decision anything made, and
    // excluding it would leave the containment rule below unable to reach the
    // very case it was written for. So model-written pairs fall through.
    if a.corpus_id.is_some()
        && a.corpus_id == b.corpus_id
        && a.segment_idx.is_some()
        && a.segment_idx == b.segment_idx
        && (a.provenance == Provenance::Passage || b.provenance == Provenance::Passage)
    {
        return Ok(false);
    }

    // No free band any more. A pair at or above `auto_supersede` used to be
    // filed as near-identical and hidden by the sweep on the score alone; it
    // now falls through to `record_pair` like everything else, where
    // `pairs_to_judge` orders by score and so asks about it first.

    // One synthesis call emitting the same passage twice: the shorter text is
    // wholly inside the longer, and both came out of the same document — the
    // same window of it, when the call repeated itself inside one window, which
    // is why the exclusion above lets a written pair through. That is a defect
    // in one artifact rather than two sources saying different things, and
    // nothing is lost by hiding it — the survivor says everything it said, Ops
    // lists it, and one press undoes it.
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

    // The other half of the same-window rule. A written pair from one window
    // was let through above so containment could reach it; containment did not
    // fire, so what is left is two rows that merely resemble each other — and
    // resemblance inside one window is what the rule at the top calls a fact
    // about how they were built. Filing it would put the window job's own
    // output in front of the judge as a merge candidate, which is the cost the
    // exclusion exists to avoid. Containment is the whole of what gets through.
    if a.corpus_id.is_some()
        && a.corpus_id == b.corpus_id
        && a.segment_idx.is_some()
        && a.segment_idx == b.segment_idx
    {
        return Ok(false);
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
    // A passage is the verbatim substrate, not a claim anyone made twice.
    // Passages under one heading are alike for how they were cut, and passages
    // from different documents on one subject are alike because the subject is
    // taught more than once — neither is duplication. The narrow rule above
    // asks for one corpus and one `segment_idx` and so covers only the first;
    // this is the same statement `run` already makes about the asking side (a
    // passage never queries), applied to the side that gets filed.
    //
    // Below the containment block on purpose: a passage wholly inside another
    // in the same corpus is still superseded, deterministically and without a
    // call. What ends here is the model question, not the hygiene.
    if a.provenance == Provenance::Passage || b.provenance == Provenance::Passage {
        return Ok(false);
    }

    // A score says two texts are alike. Duplication says one of them adds
    // nothing, and those are different claims — the comment above
    // `contains_normalized` states the distinction plainly. On a base of two
    // hundred documents teaching one subject, "alike" is the ordinary condition
    // and says nothing at all about duplication: twenty-three of the thirty-
    // three pairs the live base ever filed were two scripts covering one topic.
    //
    // So the cosine admits nothing on its own. One of two things has to hold:
    // one text is wholly inside the other, or both came out of one document.
    // Containment is what keeps the real cross-corpus case working — the same
    // document ingested twice — while two scripts that merely cover one subject
    // are refused.
    //
    // Refused, and nothing is written. Not a pair, and not a link either: a
    // link means two artifacts were *used* together, `bump_link` takes the cue
    // that bound them, and a bump derived from a cosine would put an
    // observation about text into a table whose every other row is an
    // observation about behaviour. Two documents on one subject link on their
    // own, from the first search that shows them together.
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

    /// A merge is not an anchor, and the guard has to be here and not only in
    /// `embed::mark_indexed`. The embed path withholds the arming; the sweep's
    /// repair pass arms a unit for anything that has none, which after that
    /// withholding is every merge on the base. Without this the merge is an
    /// anchor again one tick later, and files the pair that becomes the next
    /// merge.
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

    /// One passage under a corpus of its own. `seed_rows` writes `captured`
    /// rows, and the case under test is two passages from two documents.
    async fn seed_passage(core: &Core, corpus: &str, text: &str) -> String {
        let src = core.store.insert_corpus(corpus, "web", None).await.unwrap();
        core.store
            .insert_artifacts_with_provenance(
                &src.id,
                &[crate::store::artifacts::NewArtifact {
                    ordinal: 0,
                    text: text.to_string(),
                    corpus_span: None,
                    title: None,
                    category: None,
                    tags: vec![],
                    segment_idx: Some(0),
                    caveats: vec![],
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
        // The guard this joins asked for one corpus AND one `segment_idx`, so
        // it never saw two passages from two documents — on the live base it
        // fired on none of thirty-three pairs. Thirteen scripts teaching one
        // subject produced passages at 0.89 to 0.93 that duplicate nothing, and
        // they were merged into one synthetic document.
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
        // Eleven of the live base's thirty-three pairs were a passage against a
        // captured artifact, so the rule cannot be about two passages only.
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
        // Twenty-three of the live base's thirty-three pairs were this: two
        // documents covering one subject at 0.89 to 0.93, duplicating nothing.
        // Thirteen scripts agreeing is evidence that the claim is standard, and
        // a merge erases that they agreed.
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
        // The case the corroborant keeps working: one document ingested twice.
        // Containment is the predicate that says one side adds nothing, which
        // is what duplication means — a score only says they are alike.
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
        // The link graph stays an observation about use. `bump_link` takes the
        // cue that bound two artifacts, and the README calls the graph one
        // learned from co-retrieval; a bump derived from a cosine would leave
        // the recommendation surface unable to tell the two apart.
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
        // What the new guard must not take away. Containment inside one corpus
        // is deterministic, costs no call, and is the only ground on which
        // anything is hidden unasked — so the guard sits below that block, not
        // above it. Placed above, this hygiene disappears silently.
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
                        ordinal: 0,
                        text: "Spuren sind materielle Veraenderungen an Personen oder Sachen."
                            .into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(0),
                        caveats: vec![],
                    },
                    crate::store::artifacts::NewArtifact {
                        ordinal: 1,
                        text: "Spuren sind materielle Veraenderungen".into(),
                        corpus_span: None,
                        title: None,
                        category: None,
                        tags: vec![],
                        segment_idx: Some(1),
                        caveats: vec![],
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

    #[tokio::test]
    async fn two_rows_from_one_window_are_never_a_pair() {
        // A promoted artifact and the passage it left standing in the same
        // window look like a duplicate pair and are not one: overlap inside a
        // window is the window job's decision, already made. A passage on
        // either side is what marks that case, and what this pins.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str, seg: i64| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: None,
            title: Some("same heading".into()),
            category: None,
            tags: vec![],
            segment_idx: Some(seg),
            caveats: vec![],
        };
        // One written row and the verbatim passage beside it, in one window.
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
        // Relate the same-window pair only: identical text under one title
        // embeds identically, so without the exclusion this is a 1.0 pair.
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
        // Across windows of the same corpus a pair is still a question —
        // unless containment settles it, which identical text does; so the
        // cross-window pair is settled or pending, never nothing.
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
        // The case the same-window exclusion must not swallow: no passage on
        // either side, so nothing here is the window job's decision — just one
        // synthesis call that wrote the same text twice, the shorter wholly
        // inside the longer. Containment settles it and hides the copy.
        let core = test_core().await;
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let na = |o: i64, t: &str| crate::store::artifacts::NewArtifact {
            ordinal: o,
            text: t.into(),
            corpus_span: None,
            title: Some("same heading".into()),
            category: None,
            tags: vec![],
            segment_idx: Some(0),
            caveats: vec![],
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
        // Nobody is hidden…
        assert!(core.store.get_artifact(&ids[0]).await.unwrap().in_results());
        assert!(core.store.get_artifact(&ids[1]).await.unwrap().in_results());
        // …and the pair is pending, first in line by score.
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
