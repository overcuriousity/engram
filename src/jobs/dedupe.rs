//! One component, one call.
//!
//! The sweep used to make up to `max_judgements` of these in a single job, so a
//! consolidation run blocked every capture behind it for as long as twenty model
//! calls took — the second-worst blocker in the system after synthesis. The
//! sweep now decides *which* pairs are worth asking about, which costs nothing,
//! and arms one unit each. The queue paces them and interleaves them with
//! everything else.
//!
//! The unit expands its pair into the connected component of still-open pairs
//! around it. Four related artifacts settled pairwise cost three calls and write
//! two merged artifacts that are superseded almost immediately — and since a
//! re-merge is always written from captured roots, those intermediates are paid
//! for and thrown away for nothing.
//!
//! Before the call, the component is flattened to its captured roots. A merged
//! member's own text is never shown to the model: rewriting from a rewrite is
//! how a paraphrase of a paraphrase ends up three generations from the wording
//! someone actually captured. The lineage closure exists precisely so that
//! flattening costs one query, and it keeps information loss one generation deep
//! however many times a group is merged.
//!
//! Four verdicts come back and only two touch an artifact. A value conflict is
//! escalated to a person and never merged; a group past the fan-in cap is
//! surfaced rather than rewritten.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::infer::prompt::{MergedDraft, Relation};
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::pairs::{ArtifactPair, PairState};

/// What the model decided, with everything the write path needs already read.
pub struct Settlement {
    pub relation: Relation,
    pub detail: Option<String>,
    /// The root named obsolete, already checked against newest-wins. A root and
    /// not a member, because roots are what the model was shown and therefore
    /// the only things its letter can be naming. Only set for `Replaced`.
    pub obsolete: Option<String>,
    /// Only set for `Duplicate`, and only once the loss check has passed.
    pub merged: Option<MergedDraft>,
    /// The component's members, active as of the call.
    pub members: Vec<Chunk>,
    /// Their captured roots, flattened. What the model was actually shown.
    pub roots: Vec<Chunk>,
    pub pairs: Vec<ArtifactPair>,
}

pub async fn run(core: &Core, pair_id: &str) -> Result<()> {
    let id: i64 = pair_id.parse().map_err(|_| Error::NotFound)?;
    let p = core.store.get_pair(id).await?;
    if p.state != PairState::Pending {
        // Settled by an operator, by a later sweep, or by a sibling unit that
        // already answered this whole component while this one waited.
        return Ok(());
    }

    let mut pairs = core.store.open_component(id).await?;
    let mut member_ids: Vec<String> = pairs
        .iter()
        .flat_map(|p| [p.a_id.clone(), p.b_id.clone()])
        .collect();
    member_ids.sort();
    member_ids.dedup();

    let mut members = Vec::new();
    let mut retired: std::collections::HashSet<String> = Default::default();
    for mid in &member_ids {
        // Reported, not swallowed. `a_id` and `b_id` are `ON DELETE CASCADE` and
        // every pool sets `foreign_keys`, so a pair naming an artifact that is
        // gone is a state the schema does not allow — a failure here is the
        // store being unwell, not a deletion to absorb.
        let c = core.store.get_artifact(mid).await?;
        // Re-checked here and not only when the unit was armed: a member can be
        // superseded by a later sweep or deprecated by an operator while this
        // waits out a backoff, and spending the scarcest thing in the system to
        // rule on an artifact no longer in results buys nothing.
        if c.status != ArtifactStatus::Active || c.superseded_by.is_some() {
            retired.insert(c.id);
            continue;
        }
        members.push(c);
    }
    if !retired.is_empty() {
        // Only the pairs naming a retired member are answered by its
        // retirement. Dismissing the whole component killed sibling pairs
        // between still-active duplicates — record_pair is INSERT OR IGNORE
        // and Dismissed appears on no list, so nothing could ever re-file
        // them and the surviving duplication was invisible forever.
        let (dead, live): (Vec<_>, Vec<_>) = pairs
            .into_iter()
            .partition(|pr| retired.contains(&pr.a_id) || retired.contains(&pr.b_id));
        settle_all(
            core,
            &dead,
            PairState::Dismissed,
            Some("a member is no longer in results"),
        )
        .await?;
        pairs = live;
        // Dropping a member can strand others with no surviving pair; they
        // are simply not part of this unit's question any more.
        let named: std::collections::HashSet<&str> = pairs
            .iter()
            .flat_map(|pr| [pr.a_id.as_str(), pr.b_id.as_str()])
            .collect();
        members.retain(|c| named.contains(c.id.as_str()));
        // The seed pair itself may be among the dead; the survivors keep
        // their own units and nothing further is owed here.
        if !pairs.iter().any(|pr| pr.id == id) {
            return Ok(());
        }
    }
    if members.len() < 2 {
        settle_all(core, &pairs, PairState::Dismissed, None).await?;
        return Ok(());
    }

    // Flatten before anything else, and never show the model a merged member's
    // own text.
    let root_map = core.store.roots_of(&member_ids).await?;
    // A member with no roots at all is a merge whose sources were deleted out
    // from under it. Its text is a paraphrase with nothing behind it — not
    // something to show the model as an original, and not something a rule can
    // settle. A person decides.
    if members
        .iter()
        .any(|c| root_map.get(&c.id).is_none_or(|r| r.is_empty()))
    {
        settle_all(
            core,
            &pairs,
            PairState::Contradiction,
            Some("a merged member has lost its sources; resolve by hand"),
        )
        .await?;
        return Ok(());
    }
    let mut root_ids: Vec<String> = root_map.values().flatten().cloned().collect();
    root_ids.sort();
    root_ids.dedup();

    if root_ids.len() > core.consolidate.merge_max_roots {
        tracing::info!(
            pair = id,
            roots = root_ids.len(),
            cap = core.consolidate.merge_max_roots,
            "component draws on more roots than the cap; surfacing instead of merging"
        );
        let detail = format!(
            "{} sources, cap is {}",
            root_ids.len(),
            core.consolidate.merge_max_roots
        );
        settle_all(core, &pairs, PairState::Oversized, Some(&detail)).await?;
        return Ok(());
    }

    // Two members can flatten to one root — a merge and one of its own sources
    // meet this way — and one root is nothing to compare. Asking would spend a
    // call to be told an artifact matches itself.
    if root_ids.len() < 2 {
        settle_all(core, &pairs, PairState::Dismissed, None).await?;
        return Ok(());
    }

    let mut roots = Vec::new();
    for rid in &root_ids {
        roots.push(core.store.get_artifact(rid).await?);
    }

    // Counted before the call and regardless of how it goes, so a group the
    // model keeps failing on drops behind the rest of the queue rather than
    // absorbing the budget again on the next sweep.
    for pr in &pairs {
        core.store.record_judge_attempt(pr.id).await?;
    }

    let shown: Vec<(&str, &str)> = roots
        .iter()
        .map(|c| (c.title.as_deref().unwrap_or("untitled"), c.text.as_str()))
        .collect();
    let texts: Vec<&str> = roots.iter().map(|c| c.text.as_str()).collect();
    let differing = crate::infer::facts::differing_values(&texts);

    let permit = core.gate.background().await;
    let reply = match core
        .completer
        .complete(
            crate::infer::prompt::DEDUPE_SYSTEM,
            &crate::infer::prompt::dedupe_prompt(&shown, &differing, p.judge_attempts),
        )
        .await
    {
        Ok(r) => {
            permit.succeeded();
            r
        }
        Err(e) => {
            permit.failed(&e);
            return Err(e);
        }
    };

    let verdict = match crate::infer::prompt::parse_dedupe(&reply) {
        Ok(v) => v,
        // A reply that cannot be read is an error, not a verdict: the component
        // stays pending and the unit retries under the queue's backoff.
        //
        // Retrying is only worth anything because `dedupe_prompt` carries the
        // attempt number. Against an endpoint that caches by exact prompt, an
        // unchanged prompt would replay the same unreadable bytes for every one
        // of `MAX_ATTEMPTS`.
        //
        // Counted here and not beside `record_judge_attempt`, because this is
        // the only failure that says anything about the group. A call the
        // endpoint never answered says something about the endpoint, and letting
        // an outage count against every pending pair would take the whole review
        // queue out of reach on its way past.
        Err(e) => {
            for pr in &pairs {
                core.store.record_unreadable_judgement(pr.id).await?;
            }
            tracing::warn!(
                pair = id,
                attempt = p.judge_attempts,
                error = %e,
                "dedupe reply unreadable; component stays pending"
            );
            return Err(e);
        }
    };

    apply(core, interpret(verdict, members, roots, pairs)).await
}

/// Turn a parsed reply into what the write path will do.
///
/// Both guards live here because neither needs the store, which makes them
/// testable without one — and both turn a verdict *down* rather than up. This
/// pass is allowed to decline to act; it is not allowed to act on a shaky
/// answer.
fn interpret(
    v: crate::infer::prompt::Dedupe,
    members: Vec<Chunk>,
    roots: Vec<Chunk>,
    pairs: Vec<ArtifactPair>,
) -> Settlement {
    let mut relation = v.relation;
    let mut detail = v.detail;
    let mut merged = v.merged;
    let mut obsolete = None;

    if relation == Relation::Replaced {
        // Trust a named direction only when it agrees with the sweep's own
        // newest-wins bias (see `keeper`): a call naming the *newer* artifact
        // obsolete is exactly the failure mode worth guarding against, since it
        // would hide the side more likely to be current.
        //
        // The letter indexes `roots`. `run` builds the lettered list from the
        // flattened roots and never shows a merged member's own text, so a
        // letter resolved against `members` would name a different artifact
        // whenever the two lists diverge — which is exactly when the component
        // contains an earlier merge. That mismatch superseded an artifact the
        // model had never been shown.
        let named = v
            .supersedes
            .map(|c| (c as u8 - b'a') as usize)
            .and_then(|i| roots.get(i));
        obsolete = match named {
            Some(named)
                if roots
                    .iter()
                    .all(|o| o.id == named.id || named.created_at <= o.created_at) =>
            {
                Some(named.id.clone())
            }
            _ => None,
        };
        if obsolete.is_none() {
            relation = Relation::Conflict;
        }
    }

    if relation == Relation::Duplicate
        && let Some(d) = &merged
    {
        let lost = crate::jobs::merge::losses(&roots, d);
        if !lost.is_empty() {
            // Escalated rather than retried: the merge is the thing that was
            // wrong, and naming what it would have cost is a line an operator
            // can act on where "verification failed" is not.
            detail = Some(format!("the merge would have lost {}", lost.join(", ")));
            relation = Relation::Conflict;
            merged = None;
        }
    }

    Settlement {
        relation,
        detail,
        obsolete,
        merged,
        members,
        roots,
        pairs,
    }
}

async fn apply(core: &Core, s: Settlement) -> Result<()> {
    match s.relation {
        Relation::Distinct => {
            settle_all(core, &s.pairs, PairState::NoConflict, s.detail.as_deref()).await
        }
        Relation::Conflict => {
            tracing::info!(
                members = s.members.len(),
                "artifacts disagree; escalating rather than merging"
            );
            settle_all(
                core,
                &s.pairs,
                PairState::Contradiction,
                s.detail.as_deref(),
            )
            .await
        }
        Relation::Replaced => {
            let obsolete = s
                .obsolete
                .clone()
                .expect("interpret sets this or downgrades to Conflict");
            // Fresh statuses, not the snapshot `interpret` saw. The roots of a
            // member that is itself a finished merge are already superseded,
            // and a component can change while the unit waits out a backoff.
            let mut fresh = Vec::new();
            for r in &s.roots {
                match core.store.get_artifact(&r.id).await {
                    Ok(c) => fresh.push(c),
                    Err(Error::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            let live = |c: &Chunk| c.status == ArtifactStatus::Active && c.superseded_by.is_none();
            let obsolete_live = fresh.iter().any(|c| c.id == obsolete && live(c));
            // A live root wins if one exists; otherwise the live member that
            // carries the surviving roots — a finished merge's own sources are
            // superseded, and the merge is the one thing still in results.
            let winner = fresh
                .iter()
                .find(|c| c.id != obsolete && live(c))
                .map(|c| c.id.clone())
                .or_else(|| {
                    s.members
                        .iter()
                        .find(|m| m.id != obsolete)
                        .map(|m| m.id.clone())
                });
            let (Some(winner), true) = (winner, obsolete_live) else {
                // Nothing to apply: the named side is already out of results,
                // so the replacement has in effect already happened.
                return settle_all(
                    core,
                    &s.pairs,
                    PairState::NoConflict,
                    Some("the named replacement is already out of results"),
                )
                .await;
            };

            if core.consolidate.autonomous {
                // The side effect FIRST. A failure here leaves every pair
                // pending, so the unit retries under the queue's backoff — the
                // reverse order left the verdict recorded on the pairs but
                // never applied, permanently, because run() skips a component
                // whose seed is no longer Pending.
                core.supersede(&obsolete, &winner).await?;
                tracing::info!(superseded = %obsolete, by = %winner, "applied a replacement");
                for pr in &s.pairs {
                    if pr.a_id == obsolete || pr.b_id == obsolete {
                        // As the manual apply settles it (`apply_pair_supersede_ui`):
                        // done, with the model's reasoning kept as the record
                        // of why. Leaving it Superseded listed the applied
                        // replacement as awaiting confirmation forever, behind
                        // Keep buttons that could only return a validation
                        // error against the already-superseded side.
                        core.store
                            .set_pair_state(pr.id, PairState::Dismissed, s.detail.as_deref())
                            .await?;
                    } else {
                        // Both sides survived. Writing the direction here
                        // anyway named an artifact the pair does not contain.
                        // Not left pending either: the roots this verdict was
                        // drawn from are unchanged, so re-arming would build
                        // the identical prompt and receive the identical
                        // answer forever. An unanswered question goes where
                        // the others go: to a person.
                        core.store
                            .set_pair_state(
                                pr.id,
                                PairState::Contradiction,
                                Some(&format!(
                                    "{obsolete} was superseded; these two were not separated"
                                )),
                            )
                            .await?;
                    }
                }
                return Ok(());
            }

            // Proposal mode: nothing is hidden, the pair carries the direction
            // and an operator confirms via "apply supersede".
            for pr in &s.pairs {
                if pr.a_id == obsolete || pr.b_id == obsolete {
                    core.store
                        .set_pair_superseded(pr.id, &obsolete, s.detail.as_deref())
                        .await?;
                } else {
                    // Both sides survived. Writing the direction here anyway
                    // named an artifact the pair does not contain, and Ops
                    // rendered an "apply supersede" button for it — a button
                    // that would hide a third artifact on the strength of a
                    // question about two others.
                    core.store
                        .set_pair_state(
                            pr.id,
                            PairState::Contradiction,
                            Some(&format!(
                                "{obsolete} was superseded; these two were not separated"
                            )),
                        )
                        .await?;
                }
            }
            tracing::info!(obsolete = %obsolete, "proposed a replacement, pending confirmation");
            Ok(())
        }
        Relation::Duplicate => {
            let draft = s
                .merged
                .as_ref()
                .expect("interpret keeps this or downgrades to Conflict");
            if !core.consolidate.autonomous {
                // Recorded, not applied. Reading the verdicts before letting the
                // system act on them is the cheapest evidence available about
                // whether the contract holds on real data, and it is the only
                // reason the switch is a switch rather than a leap. Its own
                // state rather than Contradiction: filing a mergeable pair
                // among genuine conflicts made the UI claim the two disagree,
                // and steered the operator toward hiding a side the model
                // judged complementary. The draft is discarded — once autonomy
                // is on, the unit re-judges and merges then.
                return settle_all(core, &s.pairs, PairState::WouldMerge, s.detail.as_deref())
                    .await;
            }

            // Every member, not just the roots. A merged member is not its own
            // root, and `finish` hides what the lineage names — so passing only
            // the roots would leave that earlier merge active and near-identical
            // to the new one. `insert_merged_artifact` flattens all of them to
            // captured roots, and `subsumed_merges` catches the merged members.
            let sources: Vec<String> = s.members.iter().map(|m| m.id.clone()).collect();
            let m = crate::jobs::merge::write(core, draft, &sources).await?;
            // `merged_into` rather than a detail string: if the embed never
            // lands, the sweep's reap has to find exactly these pairs and
            // reopen them (`reap_stranded`).
            for pr in &s.pairs {
                core.store
                    .set_pair_merged(pr.id, &m.id, s.detail.as_deref())
                    .await?;
            }
            Ok(())
        }
    }
}

/// One verdict answers every pair in the component. The ones it did not answer
/// would be armed and asked about all over again, at full price, for a decision
/// that has already been made.
async fn settle_all(
    core: &Core,
    pairs: &[ArtifactPair],
    state: PairState,
    detail: Option<&str>,
) -> Result<()> {
    for p in pairs {
        core.store.set_pair_state(p.id, state, detail).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::infer::fake::ScriptedCompleter;
    use crate::jobs::consolidate::tests::{seed, seed_titled};
    use std::sync::Arc;

    /// Record a pair for two artifacts and hand back its row id.
    async fn queue_pair(core: &Core, a: &str, b: &str) -> i64 {
        core.store.record_pair(a, b, 0.91).await.unwrap();
        core.store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|p| (p.a_id == a || p.b_id == a) && (p.a_id == b || p.b_id == b))
            .expect("the pair was just recorded")
            .id
    }

    async fn disagreeing(core: &Core) -> Vec<String> {
        seed(
            core,
            &[
                ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
                ("engram needs Rust 1.30.0 to build.", [0.93, 0.37]),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn two_artifacts_about_different_subjects_are_never_merged() {
        // FAT12, FAT16 and FAT32 are near-identical in form and deliberately
        // different in content: they sit at 0.91 and every number in them
        // differs. This is where the feature fires hardest and is most wrong,
        // and it no longer merely flags them — a merge would grind a reference
        // document into one paragraph that describes none of its subjects.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"two different filesystems"}"#.into(),
        ]));
        let ids = seed_titled(
            &core,
            &[
                (
                    "FAT16 Specifications",
                    "Maximale Partitionsgröße 2 GB, 65524 Cluster.",
                    [1.0, 0.0],
                ),
                (
                    "FAT32 Specifications",
                    "32 Bit Clusternummern, 268435445 Cluster.",
                    [0.93, 0.37],
                ),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none(),
                "a reference document's sections were consolidated into each other"
            );
        }
    }

    #[tokio::test]
    async fn a_value_conflict_is_escalated_and_never_merged() {
        // Deciding which of two contradictory facts is current stays a person's
        // job. This is the one queue that expects a human, and autonomy does not
        // empty it.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"conflict","detail":"1.21.4 versus 1.30.0"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            assert_eq!(
                c.status,
                ArtifactStatus::Active,
                "a conflict hid an artifact"
            );
            assert!(c.superseded_by.is_none());
        }
    }

    #[tokio::test]
    async fn a_plain_replacement_supersedes_rather_than_merging() {
        // The survivor is a stored original with a valid span and corpus lines
        // to render beside it. That is strictly better than a rewrite, and it is
        // the path by which the fidelity thesis keeps holding under autonomy —
        // so the prompt prefers it and this pins that the code does too.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[1].as_str())
        );
        let winner = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(
            winner.provenance,
            crate::store::artifacts::Provenance::Captured,
            "a replacement wrote synthetic text where an original would do"
        );
        assert_eq!(winner.text, "engram needs Rust 1.30.0 to build.");
    }

    #[tokio::test]
    async fn a_replacement_is_only_proposed_while_autonomy_is_off() {
        // The observation window. Switched off, the pass still asks and still
        // records what it would have done -- which is the whole point: an
        // operator can read the verdicts before granting the authority to act
        // on them, rather than switching autonomy on blind.
        let mut core = test_core().await;
        core.consolidate.autonomous = false;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Superseded, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "the direction was not recorded");
        assert_eq!(found[0].obsolete_id.as_deref(), Some(ids[0].as_str()));
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "a proposal hid an artifact without being asked to"
        );
    }

    #[tokio::test]
    async fn a_pair_of_two_survivors_is_not_closed_with_someone_elses_direction() {
        // Three artifacts, two pairs, and one of the three named obsolete. The
        // pair that holds it has a direction; the pair of the other two does
        // not, and stamping the same `obsolete_id` on it put an artifact it does
        // not contain behind an "apply supersede" button in Ops.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"the first is stale"}"#.into(),
        ]));
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        let first = queue_pair(&core, &ids[0], &ids[1]).await;
        let second = queue_pair(&core, &ids[1], &ids[2]).await;

        run(&core, &first.to_string()).await.unwrap();

        // Applied (autonomy is on), so its own pair is done — Dismissed, as
        // the manual apply settles it, with the direction cleared.
        let held = core.store.get_pair(first).await.unwrap();
        assert_eq!(held.state, PairState::Dismissed);
        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[1].as_str())
        );

        let survivors = core.store.get_pair(second).await.unwrap();
        assert_eq!(
            survivors.obsolete_id, None,
            "a pair was closed naming an artifact it does not contain"
        );
        assert_eq!(
            survivors.state,
            PairState::Contradiction,
            "two artifacts the verdict never separated were quietly settled"
        );
    }

    #[tokio::test]
    async fn a_letter_names_the_root_it_was_shown_beside_not_the_nth_member() {
        // The component holds an earlier merge and one captured artifact, so the
        // members are {M, c} and the lettered roots are {a, b, c}. The two lists
        // diverge, and the model only ever saw the second — answering "b" means
        // the root b, whose text was on the screen. Resolved against the members
        // the same letter lands on the merge, which was never shown and never
        // named. Under autonomy that hides the wrong artifact outright.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"b","detail":"b is the stale one"}"#.into(),
        ]));
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 30 s", [0.99, 0.02]),
                ("timeout is thirty seconds", [0.98, 0.04]),
            ],
        )
        .await;
        // uuid v7 sorts by creation, so the roots letter as a, b, c and the
        // merge sorts after all three.
        let merged = crate::jobs::merge::write(
            &core,
            &MergedDraft {
                text: "timeout is 30 seconds".into(),
                title: None,
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &[ids[0].clone(), ids[1].clone()],
        )
        .await
        .unwrap();
        let pair = queue_pair(&core, &merged.id, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            core.store
                .get_artifact(&ids[1])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[0].as_str()),
            "the letter did not resolve to the root it was shown beside"
        );
        assert!(
            core.store
                .get_artifact(&merged.id)
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "an artifact the model was never shown was superseded"
        );
    }

    #[tokio::test]
    async fn a_replacement_naming_the_newer_artifact_is_not_trusted() {
        // A miscalibrated call proposing to hide the *newer* side disagrees with
        // the sweep's own newest-wins bias, so it falls back to a conflict
        // rather than being applied. Guessing here means hiding an artifact for
        // no stated reason.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        let ids = disagreeing(&core).await;
        // `now()` is second-grained, so two rows inserted in one test would tie,
        // and a tie is meant to pass the guard. Force b strictly newer.
        sqlx::query("UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?")
            .bind(&ids[1])
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"b","detail":"x"}"#.into(),
        ]));
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none()
            );
        }
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Contradiction, 10)
                .await
                .unwrap()
                .len(),
            1,
            "an untrusted direction must land as a conflict, not vanish"
        );
    }

    #[tokio::test]
    async fn a_merge_that_would_lose_a_value_is_escalated_with_the_reason() {
        // The loss check, from the unit's side. "the merge would have lost
        // 1.21.4" is a line an operator can act on; "verification failed" is not.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"engram needs Rust 1.30.0 to build.","tags":[],"caveats":[]}}"#
                .into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("1.21.4"),
            "the escalation did not say what would have been lost: {:?}",
            found[0].detail
        );
        for id in &ids {
            assert!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn one_call_settles_every_pair_in_the_component() {
        // A verdict that answered only its own pair would leave the siblings
        // pending, and the next sweep would arm them and ask the same question
        // again at full price.
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"three different things"}"#.into(),
        ]));
        core.completer = completer.clone();
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        queue_pair(&core, &ids[1], &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            completer.calls(),
            1,
            "the component cost more than one call"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "a sibling pair was left to be asked about again"
        );
    }

    #[tokio::test]
    async fn a_component_past_the_fan_in_cap_is_surfaced_and_never_called_about() {
        // A merge of forty sources is no longer one atomic piece of knowledge,
        // which is what an artifact is defined to be. Past the cap the honest
        // answer is to stop, not to write something nobody asked for.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.consolidate.merge_max_roots = 2;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        queue_pair(&core, &ids[1], &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 0, "an oversized component cost a call");
        let over = core
            .store
            .pairs_by_state(PairState::Oversized, 10)
            .await
            .unwrap();
        assert_eq!(over.len(), 2, "every pair in the component must be settled");
        assert_eq!(over[0].detail.as_deref(), Some("3 sources, cap is 2"));
    }

    #[tokio::test]
    async fn a_sibling_unit_for_a_settled_component_is_a_no_op() {
        // Two units are armed for one component and both run. The second must
        // find its work done rather than asking again.
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"unrelated"}"#.into(),
        ]));
        core.completer = completer.clone();
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        let first = queue_pair(&core, &ids[0], &ids[1]).await;
        let second = queue_pair(&core, &ids[1], &ids[2]).await;

        run(&core, &first.to_string()).await.unwrap();
        run(&core, &second.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 1, "the sibling unit asked again");
    }

    #[tokio::test]
    async fn a_failed_dedupe_leaves_the_component_pending() {
        // A dead endpoint must not silently clear a queue of real duplicates.
        let mut core = test_core().await;
        core.completer = Arc::new(ScriptedCompleter::new(vec!["not json".into()]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        assert!(run(&core, &pair.to_string()).await.is_err());

        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(core.store.get_pair(pair).await.unwrap().judge_unreadable, 1);
    }

    #[tokio::test]
    async fn a_component_whose_member_was_retired_is_dismissed_without_a_call() {
        let mut core = test_core().await;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;
        core.deprecate(&ids[0]).await.unwrap();

        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(completer.calls(), 0, "a retired artifact cost a call");
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_merge_that_lost_its_sources_is_never_shown_as_an_original() {
        // The self-root fallback put a model-synthesized paraphrase in the
        // prompt as a captured original, and a Duplicate verdict would then
        // record a merged artifact as root_id — paraphrase drift, one
        // generation per merge, which the lineage design exists to prevent.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        let completer = Arc::new(ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        let ids = disagreeing(&core).await;
        let m = core
            .store
            .insert_merged_artifact(
                &crate::store::artifacts::NewMerged {
                    text: "a paraphrase".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[ids[0].clone()],
            )
            .await
            .unwrap();
        // Its every source cascades away, as a corpus deletion does.
        sqlx::query("DELETE FROM artifact_sources WHERE child_id = ?")
            .bind(&m.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        let pair = queue_pair(&core, &m.id, &ids[1]).await;
        run(&core, &pair.to_string()).await.unwrap();

        assert_eq!(
            completer.calls(),
            0,
            "a rootless merge reached the model as an original"
        );
        assert_eq!(
            core.store.get_pair(pair).await.unwrap().state,
            PairState::Contradiction,
            "the component goes to a person rather than being judged on a paraphrase"
        );
    }

    #[tokio::test]
    async fn a_retired_member_dismisses_only_its_own_pairs() {
        // Dismissing the whole component killed sibling pairs between
        // still-active duplicates permanently: record_pair is INSERT OR
        // IGNORE and Dismissed appears on no list, so the surviving
        // duplication became invisible forever.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;
        let p_ab = queue_pair(&core, &ids[0], &ids[1]).await;
        let p_bc = queue_pair(&core, &ids[1], &ids[2]).await;
        core.deprecate(&ids[2]).await.unwrap();

        run(&core, &p_ab.to_string()).await.unwrap();

        assert_eq!(
            core.store.get_pair(p_bc).await.unwrap().state,
            PairState::Dismissed,
            "the pair naming the retired artifact should be dismissed"
        );
        assert_eq!(
            core.store.get_pair(p_ab).await.unwrap().state,
            PairState::NoConflict,
            "the pair between two live artifacts must still be judged, not dismissed"
        );
    }

    #[tokio::test]
    async fn an_applied_replacement_does_not_wait_for_an_operator() {
        // The pair used to stay in Superseded — the state every consumer reads
        // as "awaiting confirmation" — with Keep buttons that could only
        // return a validation error against the already-superseded side.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"a","detail":"old flag vs new flag"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "the replacement was not applied"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Superseded, 10)
                .await
                .unwrap()
                .is_empty(),
            "an applied replacement is still listed as awaiting confirmation"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Dismissed, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the applied pair should be settled the way the manual apply settles it"
        );
    }

    #[tokio::test]
    async fn a_replacement_naming_a_root_already_out_of_results_is_applied_to_the_carrier() {
        // A component holding a finished merge flattens to roots that are
        // already superseded. Applying blindly errored *after* the pairs were
        // settled, and run()'s Pending guard made the error permanent: the
        // verdict was recorded on the pairs yet never applied.
        let mut core = test_core().await;
        core.consolidate.autonomous = true;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"replaced","supersedes":"c","detail":"superseded by the merge"}"#.into(),
        ]));
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 30 s", [0.99, 0.02]),
                ("timeout was 30 seconds once", [0.98, 0.04]),
            ],
        )
        .await;
        // uuid v7 sorts by creation, so the roots letter as a, b, c.
        // Oldest, so the newest-wins guard accepts it as obsolete.
        sqlx::query("UPDATE artifacts SET created_at = created_at - 100 WHERE id = ?")
            .bind(&ids[2])
            .execute(&core.store.pool)
            .await
            .unwrap();
        let merged = crate::jobs::merge::write(
            &core,
            &MergedDraft {
                text: "timeout is 30 seconds".into(),
                title: None,
                category: None,
                tags: vec![],
                caveats: vec![],
            },
            &[ids[0].clone(), ids[1].clone()],
        )
        .await
        .unwrap();
        // The merge finished: its roots are superseded, only it and c remain.
        crate::jobs::merge::finish(&core, &merged.id).await.unwrap();
        let pair = queue_pair(&core, &merged.id, &ids[2]).await;

        run(&core, &pair.to_string()).await.unwrap();

        // Every root besides c is superseded, so the live carrier — the merge
        // itself, a member — wins, and the settle happens after the apply.
        assert_eq!(
            core.store
                .get_artifact(&ids[2])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(merged.id.as_str()),
            "the replacement was not applied against the live carrier"
        );
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "the component was left pending"
        );
    }

    #[tokio::test]
    async fn with_autonomy_off_a_duplicate_verdict_is_filed_as_would_merge() {
        // It used to be filed as Contradiction, so the UI said "These two
        // disagree" about a pair the model judged complementary, and offered
        // only the lossy keep-one buttons for it.
        let mut core = test_core().await;
        core.consolidate.autonomous = false;
        core.completer = Arc::new(ScriptedCompleter::new(vec![
            r#"{"relation":"duplicate","detail":"same claim",
                "merged":{"text":"engram needs Rust 1.21.4 and 1.30.0 to build.","tags":[],"caveats":[]}}"#
                .into(),
        ]));
        let ids = disagreeing(&core).await;
        let pair = queue_pair(&core, &ids[0], &ids[1]).await;

        run(&core, &pair.to_string()).await.unwrap();

        let found = core
            .store
            .pairs_by_state(PairState::WouldMerge, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "the verdict must land as its own state");
        assert_eq!(found[0].detail.as_deref(), Some("same claim"));
        assert!(
            core.store
                .pairs_by_state(PairState::Contradiction, 10)
                .await
                .unwrap()
                .is_empty(),
            "a mergeable pair was filed among genuine conflicts"
        );
        // Recorded, not applied: no merge written, nothing hidden.
        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            assert!(c.superseded_by.is_none());
        }
        assert!(core.store.merged_artifacts(10).await.unwrap().is_empty());
    }
}
