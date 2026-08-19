//! Consolidation: what to do about artifacts the index says are the same.
//!
//! Duplicates are found by `jobs::relate`, per artifact, as it is embedded.
//! This sweep is the backstop for that arming, the clustering that spans many
//! pairs at once, the pacing of the dedupe units, and the repairs.
//!
//! Two thresholds still divide the work. At or above `auto_supersede` a group
//! collapses onto its newest member for free: no call, no rewrite, and the
//! survivor is a stored original. Between `review_min` and that, the group goes
//! to `jobs::dedupe`, because two genuinely distinct artifacts about one
//! subsystem sit at 0.88 routinely and acting on that score alone destroys
//! knowledge rather than duplication.
//!
//! **On merging.** This header used to say that nothing here rewrites an
//! artifact, and that a merged artifact would be synthetic text standing where
//! a stored passage used to. Merging is now permitted, narrowly, and the four
//! conditions that make it safe are part of that argument rather than
//! exceptions to it:
//!
//! - Superseding is preferred wherever one stored original suffices, so most
//!   groups still produce no synthetic text at all (`Relation::Replaced`).
//! - A merged artifact is a distinct `provenance` kind naming what it was
//!   written from, never mistakable for a captured passage.
//! - The originals are superseded, never deleted — still stored, still
//!   readable, one write from active, and one button from restored.
//! - No merge may drop a value or a literal any source carried
//!   (`jobs::merge::losses`), and one that would is escalated rather than
//!   written.
//!
//! A disagreement about a value is still never settled here. It goes to a
//! person, because deciding which of two facts is current is the judgement a
//! model is worst at.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::Chunk;
use crate::store::jobs::Stage;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Outcome {
    pub superseded: usize,
    /// Pairs this sweep armed a judge unit for. The calls happen later, one
    /// unit at a time, so this counts what was asked for rather than answered.
    pub judged: usize,
}

/// Disjoint-set over artifact ids, so a run of near-identical pairs collapses
/// into the clusters it actually describes.
///
/// Resolving the pairs one at a time does not work, and the way it fails is
/// quiet: A loses to B, then B loses to C, and A is left pointing at an
/// artifact that is itself hidden. Nothing in the UI can follow that, and the
/// reader who opens A is sent to a dead end. Grouping first means every member
/// of a cluster points at the one survivor.
#[derive(Default)]
struct Clusters {
    parent: HashMap<String, String>,
}

impl Clusters {
    fn find(&mut self, x: &str) -> String {
        let p = self.parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            return p;
        }
        let root = self.find(&p);
        self.parent.insert(x.to_string(), root.clone());
        root
    }

    fn union(&mut self, x: &str, y: &str) {
        let (rx, ry) = (self.find(x), self.find(y));
        if rx != ry {
            self.parent.insert(rx, ry);
        }
    }
}

/// Which artifact of a cluster survives.
///
/// The newest by capture time, with the id as a tie-break so the answer does
/// not depend on clock resolution. Newer is the right default because the thing
/// most often re-captured is a document that has since been updated — and Ops
/// has an undo for when it is not.
fn keeper(members: &[Chunk]) -> &Chunk {
    members
        .iter()
        .max_by_key(|c| (c.created_at, c.id.as_str()))
        .expect("a cluster has at least one member")
}

/// Whether the clustering pass answered a filed near-identical pair, and with
/// what record. `known` is false when the member's row could not be read this
/// sweep — an unreadable member is not a gone member, and closing on a
/// transient store error would settle a live pair permanently, since
/// `record_settled_pair` only updates pending rows.
fn close_filed_pair(
    a_known: bool,
    b_known: bool,
    a_live: bool,
    b_live: bool,
    a_id: &str,
    b_id: &str,
) -> Option<String> {
    if !a_known || !b_known {
        return None;
    }
    match (a_live, b_live) {
        // Both still in results, so nothing here was answered — a supersede
        // that failed and warned. The pair stays filed for the next sweep.
        (true, true) => None,
        (true, false) => Some(format!("near-identical; {a_id} kept")),
        (false, true) => Some(format!("near-identical; {b_id} kept")),
        (false, false) => Some("near-identical; neither side is in results any more".to_string()),
    }
}

/// Where dedupe units sort in the queue: `claim_job` orders by
/// `(state, attempts, seq, id)` and a window's seq is its index within a
/// document, so this puts hygiene behind every piece of capture work. A large
/// ingest may delay tidying up; tidying up may not delay an ingest.
const DEDUPE_SEQ_BASE: i64 = 1_000_000;

pub(crate) fn lifecycle_row_of(c: &Chunk) -> crate::vector::LifecycleRow {
    crate::vector::LifecycleRow {
        artifact_id: c.id.clone(),
        status: c.status,
        superseded_by: c.superseded_by.clone(),
        last_verified_at: c.last_verified_at.unwrap_or(c.created_at),
    }
}

/// How many interrupted lifecycle writes one sweep finishes. The list is
/// almost always empty; the cap only bounds a sweep after a long outage.
const DRIFT_REPAIR_BATCH: usize = 5_000;

/// Make the vector store's lifecycle payloads agree with SQLite, which is the
/// source of truth for all of them.
///
/// Every lifecycle change is two writes to two stores that cannot be written
/// atomically, so each of `deprecate`, `reactivate`, `supersede` and
/// `unsupersede` can be interrupted halfway. Both resulting skews are silent
/// and neither self-corrects: a row that says deprecated behind a payload that
/// does not leaves the artifact in search results while Ops calls it retired,
/// and a payload that says deprecated behind an active row leaves it out of
/// search with no page listing it and no button that reaches it. This is the
/// only thing in the system that notices either.
///
/// Broader than `heal_dangling_supersessions`, which repairs one specific case
/// (a winner that has since been deleted) in the SQLite direction only.
///
/// Reads `lifecycle_dirty` rather than scanning. The marker is written in the
/// same UPDATE that changes `status`/`superseded_by` and cleared once the
/// payload write is acknowledged, so the work list is exactly the writes that
/// did not finish — which is almost always empty and never grows with the base.
///
/// That marker is a claim about the whole system rather than about the four
/// mutators, and it holds only while nothing else writes a lifecycle field into
/// a payload. `jobs::embed` does — a point's payload carries `status` and
/// `superseded_by` alongside its text — and it builds them from a row read
/// before the embedding call, which is why that upsert re-reads them under
/// `lifecycle_lock` and marks the artifact for the duration. Without that, an
/// embed landing after a Restore rewrites the artifact as hidden with no marker
/// anywhere, and this repair never hears about it: see
/// `embed::upsert_with_current_lifecycle`.
///
/// Returns how many artifacts it rewrote, which is a number worth asserting on:
/// a repair that fires on a base in agreement is a bug that hides behind a
/// correct end state.
pub(crate) async fn repair_lifecycle_drift(core: &Core) -> Result<usize> {
    // Under the same lock as every lifecycle transition: the repair reads
    // rows, writes payloads and clears markers, and interleaving that with a
    // payload-first reveal is exactly the sequence that hides an artifact
    // with no marker left to find it by.
    let _guard = core.lifecycle_lock.lock().await;
    let dirty = core
        .store
        .dirty_lifecycle_artifacts(DRIFT_REPAIR_BATCH)
        .await?;
    if dirty.is_empty() {
        return Ok(0);
    }
    let rows: Vec<crate::vector::LifecycleRow> = dirty.iter().map(lifecycle_row_of).collect();
    core.vectors.apply_lifecycle(&rows).await?;
    // Only after the payload write returns. Clearing first would turn a failed
    // write into permanent drift that nothing is left to notice.
    let ids: Vec<String> = dirty.iter().map(|c| c.id.clone()).collect();
    core.store.clear_lifecycle_dirty(&ids).await?;
    tracing::info!(
        repaired = rows.len(),
        "finished lifecycle writes that never reached the vector store"
    );
    Ok(rows.len())
}

pub async fn run(core: &Core) -> Result<Outcome> {
    let cfg = &core.consolidate;
    if !cfg.enabled {
        return Ok(Outcome::default());
    }

    // Finish what was started before looking for duplicates: a sweep over a
    // half-ingested corpus is judging a base that is not there yet. The repair
    // ticker runs this too, and is the one that guarantees it runs at all —
    // this call is here for the precondition, not for the repair, which is why
    // switching the sweep off no longer switches capture repair off with it.
    crate::jobs::reconcile::run(core).await?;

    // Repairs. None may take the sweep with it: each is retried every sweep,
    // and each is most likely to fail on exactly the base that needs it most.
    //
    // What is left here is merge repair, and only that: a merge exists only
    // because this sweep made one, so a base with the sweep switched off grows
    // no new unfinished merges. Everything that can go wrong without the sweep's
    // involvement moved to `background::repair_once`.
    //
    // A merge whose process died between indexing and hiding what it replaced.
    // Invisible to everything else: complete from the artifact side, absent
    // from the pair side, and only a join across the lineage says otherwise.
    match core.store.merged_with_active_roots(200).await {
        Ok(unfinished) => {
            for id in unfinished {
                if let Err(e) = crate::jobs::merge::finish(core, &id).await {
                    tracing::warn!(merged = %id, error = %e, "could not finish a merge");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not look for unfinished merges"),
    }
    // The opposite failure: a merge that will never be embedded. Its pairs
    // are already settled, its roots were never superseded, and its only
    // signal was a forever-retrying embed job.
    match core.store.stranded_merges(50).await {
        Ok(stranded) => {
            for id in stranded {
                if let Err(e) = crate::jobs::merge::reap_stranded(core, &id).await {
                    tracing::warn!(merged = %id, error = %e, "could not reap a stranded merge");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not look for stranded merges"),
    }
    // A merged artifact whose source was deleted still carries what that source
    // said, so this is not data loss — it is a claim of provenance the artifact
    // can no longer support, and saying so beats quietly showing one fewer.
    if let Err(e) = crate::jobs::merge::flag_orphans(core).await {
        tracing::warn!(error = %e, "could not flag merged artifacts that lost a source");
    }

    // Pairs the old fan-in cap refused before any call was made. There is no
    // cap now — the unit judges two artifacts at a time — so each of these is
    // simply an unanswered question, and every one of them has
    // `judge_attempts = 0`, so putting them back redoes no work and resets no
    // backoff. Runs every sweep and matches nothing once drained, which is
    // cheaper than the machinery a one-shot would need.
    match core.store.reopen_oversized().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(pairs = n, "reopened pairs the fan-in cap had refused"),
        Err(e) => tracing::warn!(error = %e, "could not reopen the pairs the cap refused"),
    }

    let mut out = Outcome::default();

    // Group everything near-identical first, and only then decide who wins.
    //
    // The stored `NearIdentical` rows are what the relate units have filed —
    // filed rather than acted on because resolving a pair where it is found
    // leaves A pointing at a B that is itself hidden.
    let filed = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::NearIdentical, 500)
        .await?;
    let mut clusters = Clusters::default();
    let mut in_a_cluster: HashSet<String> = HashSet::new();
    for p in &filed {
        clusters.union(&p.a_id, &p.b_id);
        in_a_cluster.insert(p.a_id.clone());
        in_a_cluster.insert(p.b_id.clone());
    }

    let mut members: HashMap<String, Vec<Chunk>> = HashMap::new();
    // Which clustered ids are active, and which could not be read at all.
    // Unreadable is not gone: the closing pass must not settle a pair on the
    // strength of a store that was briefly unwell.
    let mut live: HashSet<String> = HashSet::new();
    let mut unknown: HashSet<String> = HashSet::new();
    for id in &in_a_cluster {
        let c = match core.store.get_artifact(id).await {
            Ok(c) => c,
            Err(Error::NotFound) => {
                tracing::debug!(artifact_id = %id, "pair names an artifact that is gone");
                continue;
            }
            Err(e) => {
                tracing::warn!(artifact_id = %id, error = %e, "could not read a clustered artifact this sweep");
                unknown.insert(id.clone());
                continue;
            }
        };
        if !c.in_results() {
            // A retired artifact must not win a cluster and hide a live one.
            tracing::debug!(artifact_id = %id, status = c.status.as_str(), "skipping a hidden artifact");
            continue;
        }
        live.insert(id.clone());
        let root = clusters.find(id);
        members.entry(root).or_default().push(c);
    }

    let mut hidden: HashSet<String> = HashSet::new();
    for group in members.values() {
        if group.len() < 2 {
            continue;
        }
        let keep = keeper(group);
        for c in group.iter().filter(|c| c.id != keep.id) {
            // One failure does not abandon the remaining clusters, the judge
            // pass, and every pair below.
            if crate::jobs::try_supersede(core, &c.id, &keep.id, "a near-identical artifact").await
            {
                hidden.insert(c.id.clone());
                out.superseded += 1;
            }
        }
    }

    // Close the rows the clustering just answered, or the 500-row window
    // above fills with acted-on rows and a newly filed pair never enters it.
    // `NoConflict` with a detail: settled, nothing here for a person. A pair
    // with an unreadable member stays filed for the next sweep.
    for p in &filed {
        let alive = |id: &String| live.contains(id) && !hidden.contains(id);
        let Some(detail) = close_filed_pair(
            !unknown.contains(&p.a_id),
            !unknown.contains(&p.b_id),
            alive(&p.a_id),
            alive(&p.b_id),
            &p.a_id,
            &p.b_id,
        ) else {
            continue;
        };
        if let Err(e) = core
            .store
            .set_pair_state(
                p.id,
                crate::store::pairs::PairState::NoConflict,
                Some(&detail),
            )
            .await
        {
            tracing::warn!(pair = p.id, error = %e, "could not close a settled near-identical pair");
        }
    }

    // Armed, not asked: the answers arrive one unit at a time after the sweep
    // has returned. `pairs_by_state(Contradiction, ..)` is where verdicts live.
    out.judged = arm_dedupe(core).await?;

    if out.superseded > 0 || out.judged > 0 {
        tracing::info!(
            superseded = out.superseded,
            judged = out.judged,
            "consolidation sweep finished"
        );
    }
    Ok(out)
}

/// Decide which pending pairs are worth a model call, and arm one unit each.
/// Nothing here talks to a model. Returns how many were armed:
/// `max_dedupe_per_tick` bounds what one tick arms, not units in flight, so a
/// unit the queue cannot get through does not stop every other pair from
/// being judged. `live_job` arms a pair at most once.
pub(crate) async fn arm_dedupe(core: &Core) -> Result<usize> {
    // Zero is the off switch for the model: no read, no log line.
    if core.consolidate.max_dedupe_per_tick == 0 {
        return Ok(0);
    }
    let pending = core.store.pairs_to_judge(200).await?;

    let mut armed = 0usize;
    for p in pending {
        if armed >= core.consolidate.max_dedupe_per_tick {
            tracing::info!(
                budget = core.consolidate.max_dedupe_per_tick,
                "dedupe budget spent; the rest wait for the next tick"
            );
            break;
        }
        // A store error propagates; a pair whose member is gone (cascaded
        // away between the read and here) is skipped, since propagating that
        // `NotFound` would read as the sweep's own target being gone. Liveness
        // only: this triages 200 pairs to arm a handful.
        let (a_live, b_live) = match (
            core.store.artifact_in_results(&p.a_id).await?,
            core.store.artifact_in_results(&p.b_id).await?,
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => continue,
        };

        // A member retired after the pair was filed: judging it would spend a
        // call to post a verdict about an artifact no longer in results, and
        // `Core::supersede` would refuse to apply it. The unit checks again,
        // because a pair can be retired while it waits.
        if !a_live || !b_live {
            core.store
                .set_pair_state(p.id, crate::store::pairs::PairState::Dismissed, None)
                .await?;
            continue;
        }

        // A pair whose unit is still queued from an earlier tick is already
        // going to be judged; it has no attempts yet, so it would otherwise
        // lead every tick and spend the budget on itself.
        let target = p.id.to_string();
        if core.store.live_job(Stage::Dedupe, &target).await? {
            continue;
        }

        // Idle-only: re-arming a queued unit winds its `attempts` back to
        // zero, and a pair the model will not judge would never reach
        // `MAX_ATTEMPTS` and the close-out that hands it to a later tick.
        core.store
            .rearm_idle_seq(
                Stage::Dedupe,
                "pair",
                &target,
                DEDUPE_SEQ_BASE + armed as i64,
            )
            .await?;
        armed += 1;
    }
    Ok(armed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::ArtifactStatus;
    use crate::store::artifacts::NewArtifact;
    use crate::store::pairs::PairState;
    use crate::vector::{VectorPayload, VectorPoint};

    /// Sweep, then work the judge units it armed.
    ///
    /// The sweep no longer calls the model: it decides which pairs are worth
    /// asking about and arms a unit each. Tests about what the judge *said*
    /// therefore have to drive the queue too, which is what a worker does.
    async fn sweep_and_dedupe(core: &crate::core::Core) -> Outcome {
        let out = run(core).await.unwrap();
        for _ in 0..100 {
            sqlx::query("UPDATE jobs SET run_after = 0")
                .execute(&core.store.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(core).await.unwrap_or(false) {
                break;
            }
        }
        out
    }

    /// Artifacts with vectors, one row each, under `corpus` (a fresh corpus
    /// with that raw text). `title` is the subject: the dedupe prompt is
    /// built around it, so anything testing the same-subject rule needs one.
    pub(crate) async fn seed_rows(
        core: &crate::core::Core,
        corpus: &str,
        rows: &[(Option<&str>, &str, [f32; 2])],
    ) -> Vec<String> {
        let src = core.store.insert_corpus(corpus, "web", None).await.unwrap();
        let new: Vec<NewArtifact> = rows
            .iter()
            .enumerate()
            .map(|(i, (title, text, _))| NewArtifact {
                ordinal: i as i64,
                text: (*text).to_string(),
                corpus_span: None,
                title: title.map(str::to_string),
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        let points: Vec<VectorPoint> = made
            .iter()
            .zip(rows)
            .map(|(c, (title, text, v))| VectorPoint {
                vector: v.to_vec(),
                sparse: Default::default(),
                payload: VectorPayload {
                    artifact_id: c.id.clone(),
                    corpus_id: c.corpus_id.clone().unwrap_or_default(),
                    text: (*text).to_string(),
                    title: title.map(str::to_string),
                    category: None,
                    tags: vec![],
                    created_at: c.created_at,
                    last_seen_at: None,
                    hit_count: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                },
            })
            .collect();
        core.vectors.upsert(points).await.unwrap();
        made.into_iter().map(|c| c.id).collect()
    }

    /// `seed`, then what indexing does live: each artifact asks after its
    /// neighbours, and the pairs that files are what the sweep reads.
    async fn seed_related(core: &crate::core::Core, vectors: &[(&str, [f32; 2])]) -> Vec<String> {
        let ids = seed(core, vectors).await;
        for id in &ids {
            crate::jobs::relate::run(core, id).await.unwrap();
        }
        ids
    }

    pub(crate) async fn seed_titled(
        core: &crate::core::Core,
        rows: &[(&str, &str, [f32; 2])],
    ) -> Vec<String> {
        let rows: Vec<_> = rows.iter().map(|(t, x, v)| (Some(*t), *x, *v)).collect();
        seed_rows(core, "raw", &rows).await
    }

    pub(crate) async fn seed(
        core: &crate::core::Core,
        vectors: &[(&str, [f32; 2])],
    ) -> Vec<String> {
        let rows: Vec<_> = vectors.iter().map(|(x, v)| (None, *x, *v)).collect();
        seed_rows(core, "raw", &rows).await
    }

    /// One artifact under a corpus of its own, for the cases where "same
    /// document" is the thing under test.
    pub(crate) async fn seed_into_new_corpus(
        core: &crate::core::Core,
        text: &str,
        vector: [f32; 2],
    ) -> String {
        seed_rows(core, text, &[(None, text, vector)])
            .await
            .remove(0)
    }

    #[tokio::test]
    async fn a_near_identical_pair_supersedes_the_older_artifact() {
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 1, "{out:?}");

        // The older one loses: ordinal 0 was inserted first.
        let older = core.store.get_artifact(&ids[0]).await.unwrap();
        let newer = core.store.get_artifact(&ids[1]).await.unwrap();
        assert_eq!(older.superseded_by.as_deref(), Some(ids[1].as_str()));
        assert!(newer.superseded_by.is_none());

        // And it is out of search, which is the whole point.
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload.artifact_id, ids[1]);
    }

    #[tokio::test]
    async fn reactivating_a_superseded_artifact_survives_the_next_sweep() {
        // Flipping the status without clearing `superseded_by` left the row
        // still pointing at its winner, so the next sweep re-applied the
        // superseded flag and the operator's decision quietly disappeared.
        // The pair sits in the review band, below `auto_supersede`, so nothing
        // here is a near-identical copy the sweep would re-hide on merit. The
        // supersession is an applied judge proposal, which is the case an
        // operator would reverse.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();

        core.reactivate(&ids[0]).await.unwrap();

        let back = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            back.superseded_by.is_none(),
            "reactivate left the row pointing at its winner"
        );
        assert_eq!(back.status, crate::store::artifacts::ArtifactStatus::Active);

        run(&core).await.unwrap();
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "the sweep re-hid an artifact an operator had reactivated"
        );
    }

    #[tokio::test]
    async fn the_repair_pass_finishes_a_supersession_whose_payload_write_was_lost() {
        // The row and the payload cannot be written atomically. A crash between
        // them used to be permanent: the next pass skipped the artifact
        // because its row said hidden, so the flag never landed and the
        // artifact stayed listed as hidden on Ops while every search returned
        // it, forever.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        crate::core::background::repair_once(&core).await;

        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the hidden artifact is still in search: {:?}",
            hits.iter()
                .map(|h| &h.payload.artifact_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(hits[0].payload.artifact_id, ids[1]);
    }

    #[tokio::test]
    async fn a_deprecated_artifact_never_wins_a_cluster() {
        // The newest member of a cluster wins, so a *newer* artifact an
        // operator retired used to be handed the win over a live older one:
        // the loser went superseded, the winner stayed deprecated, and the
        // knowledge left search entirely. It also overwrote the operator's
        // `deprecated` status with `superseded` on the way past.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.deprecate(&ids[1]).await.unwrap();

        let out = run(&core).await.unwrap();

        assert_eq!(out.superseded, 0, "{out:?}");
        let older = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            older.superseded_by.is_none(),
            "a deprecated artifact hid a live one"
        );
        assert_eq!(
            core.store.get_artifact(&ids[1]).await.unwrap().status,
            ArtifactStatus::Deprecated,
            "the sweep overwrote an operator's deprecation"
        );
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the live artifact should be the one thing search returns"
        );
        assert_eq!(hits[0].payload.artifact_id, ids[0]);
    }

    #[tokio::test]
    async fn superseding_refuses_to_overwrite_a_deprecation() {
        // `set_superseded_by` writes `status = 'superseded'` unconditionally,
        // so this is the guard that keeps an applied judge proposal from
        // erasing what an operator decided, with nothing left to tell the two
        // apart afterwards.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();

        assert!(core.supersede(&ids[0], &ids[1]).await.is_err());
        assert!(
            core.supersede(&ids[1], &ids[0]).await.is_err(),
            "a deprecated winner would hide the loser behind something out of results"
        );
        assert_eq!(
            core.store.get_artifact(&ids[0]).await.unwrap().status,
            ArtifactStatus::Deprecated
        );
    }

    #[tokio::test]
    async fn the_repair_pass_finishes_a_deprecation_whose_payload_write_was_lost() {
        // Row written, payload not: Ops lists the artifact as deprecated while
        // every search still returns it, and the only button that row offers is
        // "Reactivate". Nothing used to notice — the old self-heal branch fired
        // only on `superseded_by`.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();

        crate::core::background::repair_once(&core).await;

        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            !hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "a deprecated artifact is still in search"
        );
    }

    #[tokio::test]
    async fn the_heal_reveals_with_the_marker_protocol_and_leaves_none_standing() {
        // Payload-first direction, so the contract is unsupersede's: mark
        // before the payload write, clear only once both stores agree. Without
        // the mark, a crash between the two writes is drift no marker ever
        // announced; and without the lock, the sweep's repair can interleave
        // and write the stale hidden state back with nothing left to notice.
        let core = test_core().await;
        let ids = seed_related(&core, &[("a text", [1.0, 0.0]), ("b text", [0.0, 1.0])]).await;
        core.supersede(&ids[0], &ids[1]).await.unwrap();

        core.delete_artifact(&ids[1]).await.unwrap(); // runs the heal

        let a = core.store.get_artifact(&ids[0]).await.unwrap();
        assert!(
            a.in_results(),
            "the heal did not restore the dangling loser"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the heal left a marker on a base in agreement"
        );
    }

    #[tokio::test]
    async fn deleting_the_survivor_puts_the_artifact_it_hid_back() {
        // `superseded_by` has no foreign key behind it. Deleting the keeper
        // left the loser pointing at nothing, hidden from search in favour of a
        // copy that no longer exists — the surviving text becomes invisible,
        // which is the loss this whole feature exists to prevent.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        run(&core).await.unwrap();
        let mut hidden = None;
        for id in &ids {
            if core
                .store
                .get_artifact(id)
                .await
                .unwrap()
                .superseded_by
                .is_some()
            {
                hidden = Some(id.clone());
            }
        }
        let hidden = hidden.expect("the sweep hid nothing");
        let keeper = ids.iter().find(|id| **id != hidden).unwrap().clone();

        core.store.delete_artifact(&keeper).await.unwrap();
        core.vectors
            .delete_artifacts(std::slice::from_ref(&keeper))
            .await
            .unwrap();
        crate::core::background::repair_once(&core).await;

        assert!(
            core.store
                .get_artifact(&hidden)
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the artifact still points at a keeper that is gone"
        );
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "the last copy never came back to search");
        assert_eq!(hits[0].payload.artifact_id, hidden);
    }

    #[tokio::test]
    async fn a_pair_whose_member_was_since_hidden_never_reaches_the_model() {
        // A model call is the scarcest thing in this system. Spending one to
        // rule on an artifact that is no longer in results buys nothing, and
        // posts a contradiction about something nobody can see.
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.judge = completer.clone();
        // Queue the pair with the judge off, so the only call this test can
        // count is the one the second sweep would make.
        let ids = disagreeing(&core).await;
        run(&core).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        // A later sweep hides one member, as a re-embed moving the score above
        // `auto_supersede` would.
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();
        core.vectors
            .set_lifecycle(
                &ids[0],
                crate::store::artifacts::ArtifactStatus::Superseded,
                Some(&ids[1]),
            )
            .await
            .unwrap();

        let out = run(&core).await.unwrap();
        assert_eq!(completer.calls(), 0, "the judge ruled on a hidden artifact");
        assert_eq!(out.judged, 0);
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "the answered pair must leave the pending queue"
        );
    }

    #[tokio::test]
    async fn a_pair_whose_member_was_deprecated_never_reaches_the_dedupe_pass() {
        // The other half of the same rule, and the one that leaves a button
        // nothing can press: a judgement can propose a supersede, Ops renders
        // "Apply supersede" for it, and `Core::supersede` refuses a deprecated
        // side — so the operator gets a validation error and the pair stays
        // pending forever. The dismissal guard used to check `superseded_by`
        // only, which a deprecation never sets.
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.judge = completer.clone();
        let ids = disagreeing(&core).await;
        run(&core).await.unwrap();

        core.deprecate(&ids[0]).await.unwrap();

        let out = run(&core).await.unwrap();
        assert_eq!(
            completer.calls(),
            0,
            "the judge ruled on a deprecated artifact"
        );
        assert_eq!(out.judged, 0);
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty(),
            "a pair naming a retired artifact must leave the pending queue"
        );
    }

    #[tokio::test]
    async fn the_marker_repair_rewrites_nothing_when_the_two_stores_agree() {
        // Both scans are capped and neither cap lines up with the other, so
        // "missing from the other list" used to read as drift. On a base with
        // more hidden artifacts than one scan returns, that reported the whole
        // scan as broken every sweep and rewrote every payload in it — a
        // permanent false alarm with write amplification behind it. Only a real
        // disagreement counts.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("first", [1.0, 0.0]),
                ("second", [0.0, 1.0]),
                ("third", [0.5, 0.5]),
            ],
        )
        .await;
        // One hidden each way, both written through the paths that keep the two
        // stores in step.
        core.deprecate(&ids[0]).await.unwrap();
        core.supersede(&ids[1], &ids[2]).await.unwrap();

        assert_eq!(
            repair_lifecycle_drift(&core).await.unwrap(),
            0,
            "the repair fired on a base that agrees with itself"
        );
    }

    #[tokio::test]
    async fn a_lifecycle_write_that_lost_its_payload_is_repaired_from_the_marker() {
        // The scan this replaced was capped at DRIFT_SCAN from both sides, and
        // autonomous merging makes hidden artifacts grow without bound — every
        // merge hides at least two, permanently. Past the cap it repaired a
        // shifting window of an ever-growing set and reported success either
        // way. The marker's work list is the writes that did not finish, which
        // is almost always empty and never grows with the base.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;

        // Row written, payload not — exactly what a crash between the two
        // leaves behind.
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        assert_eq!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .len(),
            1,
            "the row write left no marker, so nothing would ever look for it"
        );

        assert_eq!(repair_lifecycle_drift(&core).await.unwrap(), 1);

        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker outlived the repair, so every sweep would redo it"
        );
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            !hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "a deprecated artifact is still in search"
        );
    }

    #[tokio::test]
    async fn a_completed_lifecycle_change_leaves_no_marker() {
        // If the four mutators did not clear it, the repair would rewrite every
        // payload they ever touched, on every sweep, forever — write
        // amplification behind a permanently correct end state.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("first", [1.0, 0.0]),
                ("second", [0.0, 1.0]),
                ("third", [0.5, 0.5]),
            ],
        )
        .await;

        core.deprecate(&ids[0]).await.unwrap();
        core.supersede(&ids[1], &ids[2]).await.unwrap();
        core.reactivate(&ids[0]).await.unwrap();
        core.unsupersede(&ids[1]).await.unwrap();

        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "a completed lifecycle change left its marker behind"
        );
        assert_eq!(repair_lifecycle_drift(&core).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_pair_in_the_review_band_is_queued_not_superseded() {
        // 0.88 is where two genuinely distinct artifacts about one subsystem
        // routinely sit. Acting on that score destroys knowledge.
        //
        // The two state a value differently, which is what keeps them on the
        // queue at all: a pair with nothing to disagree about closes itself.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 0, "{out:?}");
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
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unrelated_pair_is_left_entirely_alone() {
        let core = test_core().await;
        seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 0, "{out:?}");
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn dedupe_units_sort_behind_capture_work() {
        // A large ingest may delay tidying up; tidying up may not delay an
        // ingest. `claim_job` orders by (state, attempts, seq, id), and a
        // window's seq is its index within a document, so dedupe starting at
        // DEDUPE_SEQ_BASE puts it behind every piece of capture work of equal
        // attempt count. Nothing is lost by a dedupe unit waiting; a document
        // stuck behind twenty model calls is the thing this system was
        // restructured to prevent.
        let core = test_core().await;
        core.store
            .enqueue(Stage::SegmentWindow, "segment", "w0")
            .await
            .unwrap();
        let ids = disagreeing(&core).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();

        assert_eq!(arm_dedupe(&core).await.unwrap(), 1);

        let first = core.store.claim_job().await.unwrap().unwrap();
        assert_eq!(
            first.stage,
            Stage::SegmentWindow,
            "a dedupe unit was claimed ahead of capture work"
        );
    }

    #[tokio::test]
    async fn the_dedupe_budget_is_per_tick_not_per_sweep() {
        // `max_judgements` bounded what one sweep armed, which was right while
        // the sweep was the only producer of pairs. The relate units file them
        // continuously now, so a number per 24-hour tick is not a budget but a
        // queue that only grows — the budget is a rate, and the ticker is what
        // applies it.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        let ids = seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.0, 1.0]),
                ("timeout is 90 seconds", [-1.0, 0.0]),
                ("timeout is 120 seconds", [0.0, -1.0]),
            ],
        )
        .await;
        // Two independent pairs, so neither is swallowed by the other's
        // component. The vectors are orthogonal so nothing else is paired.
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        core.store
            .record_pair(&ids[2], &ids[3], 0.90)
            .await
            .unwrap();

        assert_eq!(
            arm_dedupe(&core).await.unwrap(),
            1,
            "the budget was ignored"
        );
        // And the next tick reaches the other one rather than re-arming the
        // first: `live_job` skips a pair whose unit is already queued.
        assert_eq!(arm_dedupe(&core).await.unwrap(), 1);
        assert_eq!(
            arm_dedupe(&core).await.unwrap(),
            0,
            "a pair with a live unit was armed again"
        );
    }

    #[tokio::test]
    async fn a_zero_budget_asks_nothing_and_still_records_everything() {
        // The one switch that turns the model off. Finding, recording and
        // clustering pairs is free and keeps happening; only the asking stops.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 0;
        disagreeing(&core).await;

        let out = run(&core).await.unwrap();

        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the pair was not recorded"
        );
        assert_eq!(out.judged, 0, "a call was armed on a zero budget");
    }

    #[tokio::test]
    async fn the_cluster_pass_settles_a_pair_the_relate_unit_filed() {
        // The relate unit files a pair above `auto_supersede` rather than
        // acting on it, because resolving where it is found leaves A pointing
        // at a B that is itself hidden. Nothing would ever settle those rows if
        // the cluster pass read only what one sampled round trip returned —
        // and the whole reason the relate unit exists is that the sample is
        // unlikely to redraw both members together.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
        // Empty the vector store's view, so only the stored row can supply the pair.
        core.vectors.delete_artifacts(&ids).await.unwrap();

        let out = run(&core).await.unwrap();

        assert_eq!(out.superseded, 1, "{out:?}");
        assert_eq!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .as_deref(),
            Some(ids[1].as_str()),
            "the newest member should have survived"
        );
    }

    #[tokio::test]
    async fn a_pair_the_cluster_pass_answered_stops_occupying_the_window() {
        // `NearIdentical` is where a pair is filed, not where it ends. The read
        // that feeds the cluster pass is `pairs_by_state(NearIdentical, 500)`
        // ordered by score, so a row left in that state forever is not merely
        // untidy — it holds a slot. Past five hundred already-acted-on rows a
        // newly filed pair with a lower score never enters the window at all,
        // and the duplicate it names is never hidden. On a growing base that is
        // permanent and silent.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
        core.vectors.delete_artifacts(&ids).await.unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(PairState::NearIdentical, 500)
                .await
                .unwrap()
                .is_empty(),
            "an answered pair is still holding a slot in the window"
        );
        let closed = core
            .store
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].detail.as_deref(),
            Some(format!("near-identical; {} kept", ids[1]).as_str()),
            "the row does not say which side survived"
        );
    }

    #[tokio::test]
    async fn a_pair_whose_supersede_failed_stays_filed_for_the_next_sweep() {
        // The other half of the same rule. Closing a row means the question was
        // answered; a cluster whose members are all still in results answered
        // nothing, and closing it anyway would lose the duplicate for good.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
        core.vectors.delete_artifacts(&ids).await.unwrap();
        // Deprecated by an operator, so `supersede` refuses both directions and
        // the cluster pass hides nothing.
        core.deprecate(&ids[0]).await.unwrap();
        core.deprecate(&ids[1]).await.unwrap();

        run(&core).await.unwrap();

        // Neither side is in results, so the question is moot rather than open —
        // it closes, but it does not claim a survivor.
        let closed = core
            .store
            .pairs_by_state(PairState::NoConflict, 10)
            .await
            .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].detail.as_deref(),
            Some("near-identical; neither side is in results any more")
        );
    }

    #[tokio::test]
    async fn a_second_sweep_changes_nothing() {
        // The sweep runs on a timer. If it were not idempotent it would churn
        // the queue and the payload flags on every tick.
        let core = test_core().await;
        seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        run(&core).await.unwrap();
        let second = run(&core).await.unwrap();
        assert_eq!(second.superseded, 0, "{second:?}");
    }

    #[tokio::test]
    async fn an_artifact_is_never_superseded_twice() {
        // Three near-identical artifacts. Whatever survives, exactly one must,
        // and no artifact may point at one that is itself superseded — that is
        // a chain the UI cannot resolve and the reader cannot follow.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("first", [1.0, 0.0]),
                ("second", [0.9999, 0.005]),
                ("third", [0.9998, 0.01]),
            ],
        )
        .await;

        run(&core).await.unwrap();

        let mut live = 0;
        for id in &ids {
            let c = core.store.get_artifact(id).await.unwrap();
            match &c.superseded_by {
                None => live += 1,
                Some(winner) => {
                    let w = core.store.get_artifact(winner).await.unwrap();
                    assert!(
                        w.superseded_by.is_none(),
                        "{id} was superseded by {winner}, which is itself superseded"
                    );
                }
            }
        }
        assert_eq!(live, 1, "exactly one artifact should have survived");
    }

    /// Two artifacts about the same thing that give a different version.
    async fn disagreeing(core: &crate::core::Core) -> Vec<String> {
        seed_related(
            core,
            &[
                ("engram needs Rust 1.21.4 to build.", [1.0, 0.0]),
                ("engram needs Rust 1.30.0 to build.", [0.93, 0.37]),
            ],
        )
        .await
    }

    #[tokio::test]
    async fn a_pair_with_no_differing_values_is_armed_for_the_model() {
        // This asserted the opposite until 2026-08-14, and the reversal is the
        // point of the change rather than a side effect of it.
        //
        // `may_disagree` admits a pair only when both sides state values and
        // those values differ. That was right for the question it was written
        // for — "do these two contradict each other?" — and is backwards for
        // deduplication: two artifacts at 0.93 saying the same thing in
        // different words have nothing to contradict and everything to merge.
        // The old rule filed them as settled and left both in every result set,
        // so the single best merge candidate was the one case the model was
        // never shown.
        let core = test_core().await;
        seed_related(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "{out:?}"
        );
        assert_eq!(out.judged, 1, "the pair was never armed for a decision");
        assert!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .is_empty(),
            "the pair was closed by the old prefilter instead of being judged"
        );
    }

    #[tokio::test]
    async fn the_dedupe_pass_stops_at_its_budget() {
        // One sweep must not be able to occupy the GPU for an hour.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        core.judge = completer.clone();
        seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;

        sweep_and_dedupe(&core).await;
        assert_eq!(completer.calls(), 1, "the budget was ignored");
    }

    #[tokio::test]
    async fn one_synthesis_call_emitting_a_passage_twice_resolves_itself() {
        // Same corpus, same call, one text wholly inside the other. That is a
        // defect in one artifact rather than two sources disagreeing, and it
        // sat on the review queue because it scores below auto_supersede.
        let core = test_core().await;
        let ids = seed_related(
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

        run(&core).await.unwrap();
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
    async fn a_restored_passage_is_not_hidden_again_by_the_next_sweep() {
        // The containment rule is deterministic: the same two texts satisfy it
        // every time either artifact is related again — a re-embed after an
        // edit, or the sweep's arming backstop. Without a record of the first
        // decision, an operator pressing Restore was overruled by the next
        // relate unit, and on the sweep-driven build by the next tick.
        let core = test_core().await;
        let ids = seed_related(
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

        run(&core).await.unwrap();
        assert!(
            core.store
                .get_artifact(&ids[1])
                .await
                .unwrap()
                .superseded_by
                .is_some(),
            "the passage was not hidden the first time"
        );
        core.unsupersede(&ids[1]).await.unwrap();

        crate::jobs::relate::run(&core, &ids[1]).await.unwrap();
        crate::jobs::relate::run(&core, &ids[0]).await.unwrap();
        run(&core).await.unwrap();
        assert!(
            core.store
                .get_artifact(&ids[1])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the sweep overruled an operator's restore"
        );
    }

    #[tokio::test]
    async fn containment_across_two_corpora_is_left_alone() {
        // Two documents that happen to share a sentence are two sources, and
        // this is exactly the case auto_supersede refuses to act on below 0.95.
        let core = test_core().await;
        let a = seed_related(
            &core,
            &[(
                "Bind mounts attach a directory elsewhere. Use mount --bind for it.",
                [1.0, 0.0],
            )],
        )
        .await;
        let b = seed_into_new_corpus(
            &core,
            "Bind mounts attach a directory elsewhere.",
            [0.93, 0.37],
        )
        .await;

        run(&core).await.unwrap();
        for id in [&a[0], &b] {
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
    async fn a_pair_with_no_differing_values_reaches_the_queue() {
        // The sibling of the rewrite above, from the sweep's side. It used to
        // assert `queued == 0` and `closed == 1`: a pair stating no differing
        // value was filed as settled and both artifacts stayed active.
        //
        // Under deduplication that is the wrong way round. Two artifacts saying
        // the same thing in different words are what merging is for, and the
        // sweep now queues them for a decision rather than closing the question
        // nobody asked. Queued is not hidden: nothing about either artifact
        // changes until a dedupe unit has ruled.
        let core = test_core().await;
        let ids = seed_related(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "{out:?}"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
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
                "queueing a pair must not hide anything"
            );
        }
    }

    #[tokio::test]
    async fn a_pair_stating_different_values_still_waits_for_a_person() {
        let core = test_core().await;
        seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;
        let out = run(&core).await.unwrap();
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn the_sweep_makes_no_inference_call_and_arms_one_unit_per_pair() {
        // Twenty judge calls in one job was the second-worst blocker in the
        // system after synthesis: a consolidation run held every capture behind
        // it for as long as twenty calls took. The sweep now decides which
        // pairs are worth asking about — all of it local — and arms a unit each.
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        core.judge = completer.clone();
        disagreeing(&core).await;

        let out = run(&core).await.unwrap();

        assert_eq!(completer.calls(), 0, "the sweep called the model");
        assert_eq!(out.judged, 1, "no judge unit was armed: {out:?}");
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'dedupe' AND state = 'pending'",
        )
        .fetch_one(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(armed, 1);

        // And the unit, when the queue gets to it, is what makes the call.
        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(completer.calls(), 1);
    }

    #[tokio::test]
    async fn a_pair_the_model_keeps_failing_on_goes_to_the_back_of_the_queue() {
        // An unreadable reply leaves the pair pending on purpose. Ordered by
        // score alone, the same top-scoring pair would then absorb every
        // sweep's budget forever and the rest would never be judged at all.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        core.judge = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            "not json".into(),
            r#"{"relation":"conflict","detail":"30 versus 90"}"#.into(),
        ]));
        seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.999, 0.01]),
                ("timeout is 90 seconds", [0.9, 0.44]),
            ],
        )
        .await;

        sweep_and_dedupe(&core).await;
        sweep_and_dedupe(&core).await;

        let pending = core.store.pairs_to_judge(10).await.unwrap();
        assert!(
            pending.iter().all(|p| p.judge_attempts <= 1),
            "the second sweep judged the same pair again: {pending:?}"
        );
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Contradiction, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the second sweep never reached an unjudged pair"
        );
    }

    #[tokio::test]
    async fn a_pair_whose_artifact_vanished_does_not_abandon_the_rest_of_the_sweep() {
        // `pairs_to_judge` hands back a snapshot, and the arming loop is full of
        // awaits: a re-segmentation of a window, or an operator deleting an
        // artifact, lands inside it and cascades one of these pairs away between
        // the read and the read of its members.
        //
        // Propagating that `NotFound` reached `run_one`, which reads it as the
        // sweep's own target being gone — it logs the job as dropped and
        // completes it. One pair that lost a race therefore cost every pair
        // behind it a wait of up to `interval_hours`.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 5;
        let ids = seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.0, 1.0]),
                ("retry is 3 times", [-1.0, 0.0]),
                ("retry is 9 times", [0.0, -1.0]),
            ],
        )
        .await;
        // The higher score leads the snapshot, so the surviving pair is only
        // reached if the missing one did not end the loop.
        core.store
            .record_pair(&ids[0], &ids[1], 0.95)
            .await
            .unwrap();
        core.store
            .record_pair(&ids[2], &ids[3], 0.90)
            .await
            .unwrap();

        // The state the sweep observes mid-loop, held still. The cascade would
        // normally take the pair row along with the artifact, which is exactly
        // why the pragma is needed to reproduce it: what the loop is holding is a
        // pair it read *before* the delete.
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&core.store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(&ids[0])
            .execute(&core.store.pool)
            .await
            .unwrap();

        let armed = arm_dedupe(&core).await.unwrap();

        assert_eq!(
            armed, 1,
            "the pair that lost the race took the whole sweep down with it"
        );
        let target: String =
            sqlx::query_scalar("SELECT target_id FROM jobs WHERE stage = 'dedupe'")
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        let surviving = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.a_id == ids[2])
            .expect("the untouched pair is still pending");
        assert_eq!(target, surviving.id.to_string());
    }

    #[tokio::test]
    async fn a_sweep_leaves_a_dedupe_unit_that_is_already_queued_alone() {
        // Two ways this went wrong at once. Re-arming a queued unit wound its
        // `attempts` back, so a pair the model will not judge never reached
        // `MAX_ATTEMPTS` and never reached the close-out that hands it to a
        // later sweep — forever young, exactly as the reconciliation sweep used
        // to keep windows. And `pairs_to_judge` orders by `judge_attempts`, so a
        // pair still waiting for its first call leads every sweep: the budget
        // went on re-arming it while pairs recorded since never got a turn.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        // Two pairs in the review band and nothing near enough to cluster, so
        // the sweep has a second pair to reach once the first is spoken for.
        let ids = seed_related(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.0, 1.0]),
                ("retry is 3 times", [-1.0, 0.0]),
                ("retry is 9 times", [0.0, -1.0]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        core.store
            .record_pair(&ids[2], &ids[3], 0.90)
            .await
            .unwrap();

        // A sweep arms one unit. Nothing runs it — the worker is busy.
        run(&core).await.unwrap();
        let first: (String, i64) =
            sqlx::query_as("SELECT target_id, id FROM jobs WHERE stage = 'dedupe'")
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        let later = crate::store::now() + 3600;
        sqlx::query("UPDATE jobs SET attempts = 2, run_after = ? WHERE id = ?")
            .bind(later)
            .bind(first.1)
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let (attempts, run_after): (i64, i64) =
            sqlx::query_as("SELECT attempts, run_after FROM jobs WHERE id = ?")
                .bind(first.1)
                .fetch_one(&core.store.pool)
                .await
                .unwrap();
        assert_eq!(
            (attempts, run_after),
            (2, later),
            "the sweep wound a queued judge unit back to zero attempts"
        );
        let armed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE stage = 'dedupe'")
            .fetch_one(&core.store.pool)
            .await
            .unwrap();
        assert_eq!(
            armed, 2,
            "the second sweep spent its budget re-arming the pair it had already queued"
        );
    }

    #[tokio::test]
    async fn the_sweep_is_off_when_configuration_says_so() {
        let mut core = test_core().await;
        core.consolidate.enabled = false;
        seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.superseded, out.judged), (0, 0));
    }

    #[tokio::test]
    async fn a_zero_dedupe_budget_arms_nothing_and_reads_nothing() {
        // With the budget at zero every tick still ran the 200-row query and
        // logged "budget spent" — dead work on the sweep's hot path, and a
        // misleading log line. The observable half: nothing is armed and no
        // pair is touched.
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 0;
        disagreeing(&core).await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.judged, 0);
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "the pending pair must be left exactly as it was"
        );
    }

    #[test]
    fn an_unreadable_member_leaves_the_filed_pair_alone() {
        // A transient BUSY on one member used to read as "gone", closing the
        // pair as NoConflict while both artifacts were live — permanently,
        // because record_settled_pair only updates pending rows.
        assert_eq!(close_filed_pair(false, true, false, true, "a", "b"), None);
        assert_eq!(close_filed_pair(true, false, true, false, "a", "b"), None);
        // Both readable and live: genuinely unanswered, stays filed.
        assert_eq!(close_filed_pair(true, true, true, true, "a", "b"), None);
        // Known-gone or known-hidden sides do close.
        assert_eq!(
            close_filed_pair(true, true, true, false, "a", "b").as_deref(),
            Some("near-identical; a kept")
        );
        assert_eq!(
            close_filed_pair(true, true, false, true, "a", "b").as_deref(),
            Some("near-identical; b kept")
        );
        assert_eq!(
            close_filed_pair(true, true, false, false, "a", "b").as_deref(),
            Some("near-identical; neither side is in results any more")
        );
    }

    #[tokio::test]
    async fn a_merge_that_can_never_embed_is_reaped_and_its_pairs_reopened() {
        // The pairs are settled the moment the merge is written. If the embed
        // then fails permanently the merge is stranded active-but-unindexed,
        // the roots are never superseded, and the settled pairs mean the
        // duplicates can never be merged again — with a forever-retrying job
        // as the only signal.
        use crate::store::artifacts::NewMerged;
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let m = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "both".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[ids[0].clone(), ids[1].clone()],
            )
            .await
            .unwrap();
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", &m.id)
            .await
            .unwrap();
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let pid = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store
            .set_pair_merged(pid, &m.id, Some("same claim"))
            .await
            .unwrap();
        // The embed job has exhausted its retries and cannot succeed.
        sqlx::query("UPDATE jobs SET attempts = ? WHERE target_id = ?")
            .bind(crate::store::jobs::MAX_ATTEMPTS)
            .bind(&m.id)
            .execute(&core.store.pool)
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert_eq!(
            core.store.get_artifact(&m.id).await.unwrap().status,
            ArtifactStatus::Deprecated,
            "the stranded merge should be retired"
        );
        let p = core.store.get_pair(pid).await.unwrap();
        assert_eq!(
            p.state,
            PairState::Contradiction,
            "the pair goes back to a person"
        );
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                ArtifactStatus::Active
            );
        }

        // And a healthy in-flight merge is left alone.
        let m2 = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "x".into(),
                    title: None,
                    category: None,
                    tags: vec![],
                    caveats: vec![],
                },
                &[ids[0].clone(), ids[1].clone()],
            )
            .await
            .unwrap();
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", &m2.id)
            .await
            .unwrap();
        run(&core).await.unwrap();
        assert_eq!(
            core.store.get_artifact(&m2.id).await.unwrap().status,
            ArtifactStatus::Active,
            "a merge whose embed is still in flight was reaped"
        );
    }

    /// The state was terminal and reached without a call ever being made: the
    /// component flattened past the cap and every pair in it was settled before
    /// the model saw anything. Sixteen sit that way in the field.
    #[tokio::test]
    async fn the_sweep_puts_the_refused_pairs_back_in_the_queue() {
        let core = crate::core::test_support::test_core().await;
        let ids = seed(&core, &[("a text", [1.0, 0.0]), ("b text", [0.93, 0.37])]).await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.91)
            .await
            .unwrap();
        let id = core
            .store
            .pairs_by_state(crate::store::pairs::PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store
            .set_pair_state(
                id,
                crate::store::pairs::PairState::Oversized,
                Some("12 sources, cap is 8"),
            )
            .await
            .unwrap();

        run(&core).await.unwrap();

        assert!(
            core.store
                .pairs_by_state(crate::store::pairs::PairState::Oversized, 10)
                .await
                .unwrap()
                .is_empty(),
            "a pair refused before any call was made is still refused"
        );
    }
}
