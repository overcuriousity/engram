//! Consolidation: what to do about two artifacts the index says are the same.
//!
//! Three thresholds and three outcomes. At or above `auto_supersede` the pair
//! is near enough to identical that the older one is hidden — it is still
//! stored, still readable, and one write undoes it. Between `review_min` and
//! that, the pair goes on a queue for a person, because two genuinely distinct
//! artifacts about one subsystem sit at 0.88 routinely and acting on that score
//! destroys knowledge rather than duplication. Below `review_min`, nothing.
//!
//! Nothing here rewrites an artifact. A merged artifact would be synthetic text
//! standing where a stored passage used to, with no segment to verify it
//! against and no corpus lines to show beside it, which is the one failure mode
//! this design exists to avoid.

use crate::core::Core;
use crate::error::Result;
use crate::store::artifacts::{ArtifactStatus, Chunk};
use crate::store::jobs::Stage;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Outcome {
    pub examined: usize,
    pub superseded: usize,
    pub queued: usize,
    /// Pairs settled without asking anyone, because the two artifacts state no
    /// value differently and so have nothing to disagree about.
    pub closed: usize,
    /// Pairs this sweep armed a judge unit for. The calls happen later, one
    /// unit at a time, so this counts what was asked for rather than answered.
    pub judged: usize,
    pub contradictions: usize,
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
/// Is the whole of one artifact inside the other, whitespace aside?
///
/// Not a similarity — containment. A score says two texts are alike; this says
/// one of them adds nothing, which is the only ground on which the sweep hides
/// something below `auto_supersede`.
fn contains_normalized(long: &str, short: &str) -> bool {
    let n = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    !short.trim().is_empty() && n(long).contains(&n(short))
}

fn keeper(members: &[Chunk]) -> &Chunk {
    members
        .iter()
        .max_by_key(|c| (c.created_at, c.id.as_str()))
        .expect("a cluster has at least one member")
}

/// How many hidden artifacts the drift repair compares per sweep, from either
/// side. A base with more than this many hidden artifacts drifting at once is
/// not a case worth unbounded scanning for; the next sweep continues.
const DRIFT_SCAN: usize = 5_000;

fn lifecycle_row_of(c: &Chunk) -> crate::vector::LifecycleRow {
    crate::vector::LifecycleRow {
        artifact_id: c.id.clone(),
        status: c.status,
        superseded_by: c.superseded_by.clone(),
        last_verified_at: c.last_verified_at.unwrap_or(c.created_at),
    }
}

/// Make the vector store's lifecycle payloads agree with SQLite, which is the
/// source of truth for all of them.
///
/// Every lifecycle change is two writes to two stores that cannot be written
/// atomically, so each of `deprecate`, `reactivate`, `supersede` and
/// `unsupersede` can be interrupted halfway. Both resulting skews are silent
/// and neither self-corrects: a row that says deprecated behind a payload that
/// does not leaves the artifact in search results while Ops calls it retired,
/// and a payload that says deprecated behind an active row leaves it out of
/// search with no page listing it and no button that reaches it. The pair of
/// scans below is the only thing in the system that notices either.
///
/// Broader than `heal_dangling_supersessions`, which repairs one specific case
/// (a winner that has since been deleted) in the SQLite direction only.
///
/// Returns how many artifacts it rewrote, which is a number worth asserting on:
/// a repair that fires on a base in agreement is a bug that hides behind a
/// correct end state.
async fn repair_lifecycle_drift(core: &Core) -> Result<usize> {
    repair_lifecycle_drift_scanning(core, DRIFT_SCAN).await
}

/// The above with its cap as a parameter, so a test can reproduce what the two
/// truncated scans do to each other without seeding `DRIFT_SCAN` artifacts.
async fn repair_lifecycle_drift_scanning(core: &Core, scan: usize) -> Result<usize> {
    // Both scans are capped, and neither cap lines up with the other:
    // `list_non_active_artifacts` returns the newest rows while `non_active_ids`
    // scrolls in point-id order. So set membership across the two proves
    // nothing — on a base with more hidden artifacts than `DRIFT_SCAN` the lists
    // barely intersect, and treating "missing from the other list" as drift
    // reported the whole scan as broken every sweep and rewrote every payload in
    // it. The union of the two scans names the artifacts worth *looking at*;
    // what each store actually says about them is then read per id, and only a
    // real disagreement is repaired.
    let store_hidden = core.store.list_non_active_artifacts(scan).await?;
    let payload_hidden = core.vectors.non_active_ids(scan).await?;

    let mut ids: Vec<String> = store_hidden.iter().map(|c| c.id.clone()).collect();
    ids.extend(payload_hidden);
    ids.sort_unstable();
    ids.dedup();

    let stored = core.vectors.lifecycle_of(&ids).await?;
    let known: HashMap<&str, &Chunk> = store_hidden.iter().map(|c| (c.id.as_str(), c)).collect();

    let mut rows = Vec::new();
    for id in &ids {
        // The row is already in hand for everything the SQLite scan returned;
        // only an id that came from the payload side alone costs a read.
        let fetched;
        let chunk = match known.get(id.as_str()) {
            Some(c) => *c,
            // An id the vector store still lists but SQLite has dropped is
            // ordinary — a delete can lag a sweep — and not an error.
            None => match core.store.get_artifact(id).await {
                Ok(c) => {
                    fetched = c;
                    &fetched
                }
                Err(_) => {
                    tracing::debug!(artifact_id = %id, "hidden point names an artifact that is gone");
                    continue;
                }
            },
        };
        // No point at all is not drift: a freshly captured artifact is hidden
        // in SQLite before its embedding job has written anything to hide.
        let Some(p) = stored.get(id) else {
            continue;
        };
        // SQLite is the source of truth for both fields. Comparing them rather
        // than just "hidden or not" also catches the subtler skew — a payload
        // that says superseded behind a row that says deprecated, or one
        // pointing at a winner the row no longer names.
        if p.status != chunk.status || p.superseded_by != chunk.superseded_by {
            rows.push(lifecycle_row_of(chunk));
        }
    }

    if !rows.is_empty() {
        tracing::info!(
            repaired = rows.len(),
            "lifecycle state disagreed between sqlite and the vector store"
        );
        core.vectors.apply_lifecycle(&rows).await?;
    }
    Ok(rows.len())
}

pub async fn run(core: &Core) -> Result<Outcome> {
    // Retention used to ride along here. It has its own ticker now
    // (`spawn_retention_ticker`): riding along meant that switching duplicate
    // hygiene off silently switched off the operator's instruction about how
    // long their query log is kept, which is not consolidation's call to make.
    let cfg = &core.consolidate;
    if !cfg.enabled {
        return Ok(Outcome::default());
    }

    // Finish what was started before looking for duplicates: a sweep over a
    // half-ingested corpus is judging a base that is not there yet.
    crate::jobs::reconcile::run(core).await?;

    // Deletions clear these as they happen; the sweep repeats it because a
    // hidden artifact pointing at nothing is invisible to search and to every
    // page that could put it back, and nothing else would ever notice.
    // Neither repair is allowed to take the sweep with it. Both are maintenance
    // over state the rest of the sweep does not read, both are retried on every
    // sweep, and both are most likely to fail on exactly the base that needs
    // them most — the one that drifted far enough to make the repair large.
    // Propagating the error stopped consolidation *permanently*: the next sweep
    // reached the same call and failed the same way, so near-duplicate
    // detection and judging never ran again.
    if let Err(e) = core.heal_dangling_supersessions().await {
        tracing::warn!(
            error = %e,
            "could not restore every artifact whose winner was deleted; retrying on the next sweep"
        );
    }
    if let Err(e) = repair_lifecycle_drift(core).await {
        tracing::warn!(
            error = %e,
            "could not reconcile lifecycle state with the vector store; retrying on the next sweep"
        );
    }
    // Startup heals too, but a process that stays up for weeks is exactly the
    // one whose stores drift: every interrupted write between the two happens
    // while it is running, not while it is starting.
    if let Err(e) = core.heal_store_drift().await {
        tracing::warn!(
            error = %e,
            "could not reconcile which artifacts the two stores hold; retrying on the next sweep"
        );
    }

    let pairs = core
        .vectors
        .near_pairs(cfg.sample, cfg.per_point, cfg.review_min)
        .await?;
    let mut out = Outcome {
        examined: pairs.len(),
        ..Default::default()
    };

    // Group everything near-identical first, and only then decide who wins.
    let mut clusters = Clusters::default();
    let mut in_a_cluster: HashSet<String> = HashSet::new();
    for p in pairs.iter().filter(|p| p.score >= cfg.auto_supersede) {
        clusters.union(&p.a, &p.b);
        in_a_cluster.insert(p.a.clone());
        in_a_cluster.insert(p.b.clone());
    }

    let mut members: HashMap<String, Vec<Chunk>> = HashMap::new();
    for id in &in_a_cluster {
        // An artifact the vector store still lists but SQLite has dropped is
        // ordinary: a delete can lag a sweep. It is not an error.
        let Ok(c) = core.store.get_artifact(id).await else {
            tracing::debug!(artifact_id = %id, "pair names an artifact that is gone");
            continue;
        };
        if c.status != ArtifactStatus::Active || c.superseded_by.is_some() {
            // The row says hidden but the vector store still offered this point
            // as a pair candidate, so its payload never caught up — the write
            // was interrupted, and `repair_lifecycle_drift` at the top of this
            // sweep has already re-issued it. Either way the artifact takes no
            // part in this run: a retired artifact must not win a cluster and
            // hide a live one, and a resolved pair has nothing left to decide.
            tracing::debug!(artifact_id = %id, status = c.status.as_str(), "skipping a hidden artifact");
            continue;
        }
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
            // SQLite first: it is the source of truth, and a payload flag with
            // no row behind it is a hidden artifact nothing can explain. See
            // `Core::supersede`.
            //
            // One failure does not abandon the rest, as in
            // `heal_dangling_supersessions`. `supersede` refuses a side that is
            // no longer active, and these statuses were read earlier in this
            // same sweep — so an operator deprecating one of them from Ops in
            // the meantime is an ordinary race, not a reason to abandon the
            // remaining clusters, the judge pass, and every pair below.
            if let Err(e) = core.supersede(&c.id, &keep.id).await {
                tracing::warn!(
                    superseded = %c.id,
                    by = %keep.id,
                    error = %e,
                    "could not hide a near-identical artifact; it stays active"
                );
                continue;
            }
            hidden.insert(c.id.clone());
            out.superseded += 1;
            tracing::info!(
                superseded = %c.id,
                by = %keep.id,
                "hid a near-identical artifact"
            );
        }
    }

    // The review band. A pair whose members were just hidden has already been
    // answered, and queueing it would ask an operator to rule on an artifact
    // that is no longer in results.
    for p in pairs.iter().filter(|p| p.score < cfg.auto_supersede) {
        if hidden.contains(&p.a) || hidden.contains(&p.b) {
            continue;
        }
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&p.a).await,
            core.store.get_artifact(&p.b).await,
        ) else {
            continue;
        };
        // Same rule as the cluster pass: only two live artifacts have a
        // question worth a queue slot, an inference call, or a containment
        // supersede below.
        if [&a, &b]
            .iter()
            .any(|c| c.status != ArtifactStatus::Active || c.superseded_by.is_some())
        {
            continue;
        }

        // One synthesis call emitting the same passage twice: the shorter text
        // is wholly inside the longer, and both came out of the same document.
        // That is a defect in one artifact rather than two sources saying
        // different things, and nothing is lost by hiding it — the survivor
        // says everything it said, Ops lists it, and one press undoes it.
        //
        // Same corpus is the whole of the condition. Two documents that share a
        // sentence are two sources, and hiding one of those on a 0.9 similarity
        // is what `auto_supersede` deliberately refuses to do.
        if a.corpus_id == b.corpus_id {
            let (long, short) = if a.text.len() >= b.text.len() {
                (&a, &b)
            } else {
                (&b, &a)
            };
            if contains_normalized(&long.text, &short.text) {
                // Same race, same treatment as the cluster pass above.
                if let Err(e) = core.supersede(&short.id, &long.id).await {
                    tracing::warn!(
                        superseded = %short.id,
                        by = %long.id,
                        error = %e,
                        "could not hide a duplicated passage; it stays active"
                    );
                    continue;
                }
                out.superseded += 1;
                tracing::info!(
                    superseded = %short.id,
                    by = %long.id,
                    "hid a passage one synthesis call emitted twice"
                );
                continue;
            }
        }

        // Two artifacts that state no value differently have nothing for a
        // person to rule on, and asking anyway is what turned this queue into a
        // list of chores. The pair is filed as settled — the sweep re-finds it
        // every run, so it has to be remembered — and both artifacts stay
        // exactly where they are. Closing a question is not hiding an answer.
        //
        // The prefilter used to run only when the judge was enabled, which it
        // is not by default, so the cheap answer was reached only by bases
        // already paying for the expensive one.
        // Both writes below warn and carry on, like the supersede calls above:
        // a pair is one row about two artifacts, and a transient BUSY on it is
        // no reason to abandon the rest of the band, the judge pass and the
        // sweep's tally. The sweep re-finds the pair next run.
        if !crate::infer::facts::may_disagree(&a.text, &b.text) {
            match core
                .store
                .record_settled_pair(
                    &p.a,
                    &p.b,
                    p.score,
                    crate::store::pairs::PairState::NoConflict,
                )
                .await
            {
                Ok(true) => {
                    out.closed += 1;
                    tracing::debug!(a = %p.a, b = %p.b, score = p.score, "pair states nothing differently");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(a = %p.a, b = %p.b, error = %e, "could not file a settled pair; it will be re-examined next sweep");
                }
            }
            continue;
        }

        match core.store.record_pair(&p.a, &p.b, p.score).await {
            Ok(true) => {
                out.queued += 1;
                tracing::info!(a = %p.a, b = %p.b, score = p.score, "queued a pair for review");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(a = %p.a, b = %p.b, error = %e, "could not queue a pair for review; it will be re-examined next sweep");
            }
        }
    }

    if cfg.judge {
        // Armed, not asked. `judged` counts the calls this sweep is responsible
        // for rather than the calls it made, since it now makes none;
        // `contradictions` is no longer knowable here at all, because the answer
        // arrives one unit at a time long after the sweep has returned.
        out.judged = arm_judgements(core).await?;
    }

    if out.superseded > 0 || out.queued > 0 || out.judged > 0 {
        tracing::info!(
            examined = out.examined,
            superseded = out.superseded,
            queued = out.queued,
            judged = out.judged,
            contradictions = out.contradictions,
            "consolidation sweep finished"
        );
    }
    Ok(out)
}

/// Ask the model about pending pairs, but only the ones that could possibly
/// disagree, and only up to the sweep's budget.
///
/// Returns how many calls were made and how many found a contradiction. A
/// failed call leaves its pair pending on purpose: a dead endpoint must never
/// look like a clean bill of health.
/// Decide which pending pairs are worth a model call, and arm one unit each.
///
/// Every filter here is local — an artifact's status, and the fact-token
/// prefilter that is the whole economic argument for this feature: most near
/// pairs have no value in common to disagree about, and a call is minutes on
/// this hardware. What survives becomes a `Judge` unit. Nothing in this sweep
/// talks to a model any more, which is what stops one consolidation run
/// blocking every capture behind it for as long as twenty calls take.
///
/// Returns how many were armed. `max_judgements` is a cap on that rather than a
/// loop bound, so the meaning of the setting is unchanged: the most model calls
/// one sweep can be responsible for.
async fn arm_judgements(core: &Core) -> Result<usize> {
    let pending = core.store.pairs_to_judge(200).await?;

    let mut armed = 0usize;
    for p in pending {
        if armed >= core.consolidate.max_judgements {
            tracing::info!(
                budget = core.consolidate.max_judgements,
                "judge budget spent; the rest wait for the next sweep"
            );
            break;
        }
        let (Ok(a), Ok(b)) = (
            core.store.get_artifact(&p.a_id).await,
            core.store.get_artifact(&p.b_id).await,
        ) else {
            continue;
        };

        // A pair queued in the review band can have a member retired after the
        // fact: superseded by a later sweep once a re-embed moves the score
        // above `auto_supersede`, or deprecated by an operator. Judging it would
        // spend the scarcest thing here to post a contradiction about an
        // artifact that is no longer in results.
        //
        // The status half matters beyond the wasted call. A judgement can
        // propose a supersede, which Ops renders as an "apply supersede" button
        // — and `Core::supersede` refuses a deprecated side, so pressing it
        // returns a validation error and the pair stays pending forever. The
        // same guard runs in `run`'s cluster pass and review band, and again in
        // the unit itself, because a pair can be retired while it waits.
        if a.status != ArtifactStatus::Active
            || b.status != ArtifactStatus::Active
            || a.superseded_by.is_some()
            || b.superseded_by.is_some()
        {
            core.store
                .set_pair_state(p.id, crate::store::pairs::PairState::Dismissed, None)
                .await?;
            continue;
        }

        if !crate::infer::facts::may_disagree(&a.text, &b.text) {
            core.store
                .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, None)
                .await?;
            continue;
        }

        // A pair whose unit is still queued from an earlier sweep is already
        // going to be judged. `pairs_to_judge` orders by `judge_attempts`, and a
        // pair that has not run yet still has none, so it leads every sweep
        // until it does — spending the budget on itself over and over while
        // pairs recorded since never get a turn.
        let target = p.id.to_string();
        if core.store.live_job(Stage::Judge, &target).await? {
            continue;
        }

        // `seq` is the pair's position in this sweep. Left at zero, twenty
        // judge units would all sort ahead of every document's second window.
        //
        // Idle-only for the same reason the reconciliation sweep is: re-arming
        // a queued unit winds its `attempts` back to zero, and a pair the model
        // will not judge would then never reach `MAX_ATTEMPTS` — never reaching
        // the close-out in `run_one` that hands it back to a later sweep. The
        // guard above already skips those; this is what keeps it true if the
        // two ever drift.
        core.store
            .rearm_idle_seq(Stage::Judge, "pair", &target, armed as i64)
            .await?;
        armed += 1;
    }
    Ok(armed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::pairs::PairState;
    use crate::vector::{VectorPayload, VectorPoint};

    /// Sweep, then work the judge units it armed.
    ///
    /// The sweep no longer calls the model: it decides which pairs are worth
    /// asking about and arms a unit each. Tests about what the judge *said*
    /// therefore have to drive the queue too, which is what a worker does.
    async fn sweep_and_judge(core: &crate::core::Core) -> Outcome {
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

    /// Seed artifacts with hand-placed vectors, so the test controls the exact
    /// similarity rather than depending on what the fake embedder produces.
    async fn seed(core: &crate::core::Core, vectors: &[(&str, [f32; 2])]) -> Vec<String> {
        let src = core.store.insert_corpus("raw", "web", None).await.unwrap();
        let new: Vec<NewArtifact> = vectors
            .iter()
            .enumerate()
            .map(|(i, (text, _))| NewArtifact {
                ordinal: i as i64,
                text: (*text).to_string(),
                corpus_span: None,
                title: None,
                category: None,
                tags: vec![],
                segment_idx: None,
                caveats: vec![],
            })
            .collect();
        let made = core.store.insert_artifacts(&src.id, &new).await.unwrap();
        let points: Vec<VectorPoint> = made
            .iter()
            .zip(vectors)
            .map(|(c, (text, v))| VectorPoint {
                vector: v.to_vec(),
                sparse: Default::default(),
                payload: VectorPayload {
                    artifact_id: c.id.clone(),
                    corpus_id: c.corpus_id.clone(),
                    text: (*text).to_string(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: c.created_at,
                    last_seen_at: None,
                    hit_count: None,
                    superseded: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                },
            })
            .collect();
        core.vectors.upsert(points).await.unwrap();
        made.into_iter().map(|c| c.id).collect()
    }

    /// One artifact under a corpus of its own, for the cases where "same
    /// document" is the thing under test.
    async fn seed_into_new_corpus(
        core: &crate::core::Core,
        text: &str,
        vector: [f32; 2],
    ) -> String {
        let src = core.store.insert_corpus(text, "web", None).await.unwrap();
        let made = core
            .store
            .insert_artifacts(
                &src.id,
                &[NewArtifact {
                    ordinal: 0,
                    text: text.to_string(),
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
        core.vectors
            .upsert(vec![VectorPoint {
                vector: vector.to_vec(),
                sparse: Default::default(),
                payload: VectorPayload {
                    artifact_id: made[0].id.clone(),
                    corpus_id: made[0].corpus_id.clone(),
                    text: text.to_string(),
                    title: None,
                    category: None,
                    tags: vec![],
                    created_at: made[0].created_at,
                    last_seen_at: None,
                    hit_count: None,
                    superseded: None,
                    status: None,
                    last_verified_at: None,
                    superseded_by: None,
                },
            }])
            .await
            .unwrap();
        made[0].id.clone()
    }

    #[tokio::test]
    async fn a_near_identical_pair_supersedes_the_older_artifact() {
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;

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
        let ids = seed(
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
    async fn a_sweep_finishes_a_supersession_whose_payload_write_was_lost() {
        // The row and the payload cannot be written atomically. A crash between
        // them used to be permanent: the next sweep skipped the artifact
        // because its row said hidden, so the flag never landed and the
        // artifact stayed listed as hidden on Ops while every search returned
        // it, forever.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .set_superseded_by(&ids[0], Some(&ids[1]))
            .await
            .unwrap();

        run(&core).await.unwrap();

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
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
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
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
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
    async fn the_sweep_finishes_a_deprecation_whose_payload_write_was_lost() {
        // Row written, payload not: Ops lists the artifact as deprecated while
        // every search still returns it, and the only button that row offers is
        // "Reactivate". Nothing used to notice — the old self-heal branch fired
        // only on `superseded_by`.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();

        run(&core).await.unwrap();

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
    async fn the_sweep_frees_an_artifact_hidden_by_a_payload_alone() {
        // The other skew, and the worse one: the payload says deprecated but
        // the row says active, so the artifact is out of search, off the Ops
        // deprecated list, and out of reach of every button — invisible and
        // unrecoverable until something reconciles the two stores.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.vectors
            .set_lifecycle(&ids[0], ArtifactStatus::Deprecated, None)
            .await
            .unwrap();

        run(&core).await.unwrap();

        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.payload.artifact_id == ids[0]),
            "an artifact SQLite calls active is still hidden from search"
        );
    }

    #[tokio::test]
    async fn deleting_the_survivor_puts_the_artifact_it_hid_back() {
        // `superseded_by` has no foreign key behind it. Deleting the keeper
        // left the loser pointing at nothing, hidden from search in favour of a
        // copy that no longer exists — the surviving text becomes invisible,
        // which is the loss this whole feature exists to prevent.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
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
        run(&core).await.unwrap();

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
        core.completer = completer.clone();
        // Queue the pair with the judge off, so the only call this test can
        // count is the one the second sweep would make.
        let ids = disagreeing(&core).await;
        run(&core).await.unwrap();
        core.consolidate.judge = true;
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
        core.vectors.set_superseded(&ids[0], true).await.unwrap();

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
    async fn a_pair_whose_member_was_deprecated_never_reaches_the_judge() {
        // The other half of the same rule, and the one that leaves a button
        // nothing can press: a judgement can propose a supersede, Ops renders
        // "Apply supersede" for it, and `Core::supersede` refuses a deprecated
        // side — so the operator gets a validation error and the pair stays
        // pending forever. The dismissal guard used to check `superseded_by`
        // only, which a deprecation never sets.
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        let ids = disagreeing(&core).await;
        run(&core).await.unwrap();
        core.consolidate.judge = true;

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
    async fn the_drift_repair_rewrites_nothing_when_the_two_stores_agree() {
        // Both scans are capped and neither cap lines up with the other, so
        // "missing from the other list" used to read as drift. On a base with
        // more hidden artifacts than one scan returns, that reported the whole
        // scan as broken every sweep and rewrote every payload in it — a
        // permanent false alarm with write amplification behind it. Only a real
        // disagreement counts.
        let core = test_core().await;
        let ids = seed(
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
    async fn a_scan_cap_reached_from_both_sides_is_not_drift() {
        // The bug in miniature. The two scans are capped independently and
        // ordered differently — newest rows on one side, point order on the
        // other — so past the cap they name largely different artifacts. Reading
        // "absent from the other list" as drift then reports both whole scans as
        // broken on every sweep and rewrites every payload in them. Two hidden
        // artifacts and a cap of one reproduces exactly that.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();
        core.deprecate(&ids[1]).await.unwrap();

        assert_eq!(
            repair_lifecycle_drift_scanning(&core, 1).await.unwrap(),
            0,
            "the edge of a page was reported as a disagreement"
        );
    }

    #[tokio::test]
    async fn the_drift_repair_notices_a_payload_naming_the_wrong_status() {
        // Not just hidden-or-not: a payload that says superseded behind a row
        // that says deprecated is drift too, and comparing set membership alone
        // never saw it.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.deprecate(&ids[0]).await.unwrap();
        core.vectors
            .set_lifecycle(&ids[0], ArtifactStatus::Superseded, Some(&ids[1]))
            .await
            .unwrap();

        assert_eq!(repair_lifecycle_drift(&core).await.unwrap(), 1);
        let stored = core
            .vectors
            .lifecycle_of(std::slice::from_ref(&ids[0]))
            .await
            .unwrap();
        assert_eq!(stored[&ids[0]].status, ArtifactStatus::Deprecated);
        assert_eq!(stored[&ids[0]].superseded_by, None);
    }

    #[tokio::test]
    async fn a_pair_in_the_review_band_is_queued_not_superseded() {
        // 0.88 is where two genuinely distinct artifacts about one subsystem
        // routinely sit. Acting on that score destroys knowledge.
        //
        // The two state a value differently, which is what keeps them on the
        // queue at all: a pair with nothing to disagree about closes itself.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 0, "{out:?}");
        assert_eq!(out.queued, 1);
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
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.superseded, out.queued), (0, 0), "{out:?}");
    }

    #[tokio::test]
    async fn a_second_sweep_changes_nothing() {
        // The sweep runs on a timer. If it were not idempotent it would churn
        // the queue and the payload flags on every tick.
        let core = test_core().await;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        run(&core).await.unwrap();
        let second = run(&core).await.unwrap();
        assert_eq!((second.superseded, second.queued), (0, 0), "{second:?}");
    }

    #[tokio::test]
    async fn an_artifact_is_never_superseded_twice() {
        // Three near-identical artifacts. Whatever survives, exactly one must,
        // and no artifact may point at one that is itself superseded — that is
        // a chain the UI cannot resolve and the reader cannot follow.
        let core = test_core().await;
        let ids = seed(
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
    async fn the_judge_is_off_by_default() {
        let core = test_core().await;
        disagreeing(&core).await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 1);
        assert_eq!(out.judged, 0, "the judge ran without being asked for");
    }

    #[tokio::test]
    async fn an_enabled_judge_marks_a_real_contradiction() {
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"contradicts":true,"detail":"1.21.4 versus 1.30.0"}"#.into(),
        ]));
        disagreeing(&core).await;

        let out = sweep_and_judge(&core).await;
        assert_eq!(out.judged, 1, "one pair should have been armed: {out:?}");
        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
    }

    #[tokio::test]
    async fn a_confident_direction_proposes_a_supersede_but_does_not_apply_it() {
        // The judge names a direction; an operator still has to confirm it.
        // Nothing about either artifact changes here — only the pair.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"contradicts":true,"detail":"old flag vs new flag","obsolete":"a"}"#.into(),
        ]));
        let ids = disagreeing(&core).await;

        let out = sweep_and_judge(&core).await;
        assert_eq!(out.judged, 1, "one pair should have been armed: {out:?}");
        let found = core
            .store
            .pairs_by_state(PairState::Superseded, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "the pair did not land as proposed");
        assert_eq!(found[0].obsolete_id.as_deref(), Some(ids[0].as_str()));

        // Proposed, not applied.
        assert!(
            core.store
                .get_artifact(&ids[0])
                .await
                .unwrap()
                .superseded_by
                .is_none(),
            "the judge's proposal must not hide anything by itself"
        );
    }

    #[tokio::test]
    async fn a_direction_naming_the_newer_artifact_is_not_trusted() {
        // A miscalibrated call proposing to hide the *newer* side is exactly
        // the failure mode the newest-wins guard exists to catch: it disagrees
        // with the sweep's own bias, so it must fall back to a plain
        // contradiction rather than being trusted.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        let ids = disagreeing(&core).await;
        // Force `b` (ids[1]) strictly newer than `a`: `now()` is second-grained,
        // so two rows inserted in the same test would otherwise tie, and a tie
        // is meant to pass the guard. Naming the genuinely newer side obsolete
        // disagrees with `keeper()`'s bias and must be rejected.
        sqlx::query("UPDATE artifacts SET created_at = created_at + 100 WHERE id = ?")
            .bind(&ids[1])
            .execute(&core.store.pool)
            .await
            .unwrap();
        core.completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"contradicts":true,"detail":"x","obsolete":"b"}"#.into(),
        ]));

        run(&core).await.unwrap();
        let superseded = core
            .store
            .pairs_by_state(PairState::Superseded, 10)
            .await
            .unwrap();
        assert!(
            superseded.is_empty(),
            "a direction naming the newer artifact must not be trusted: {superseded:?}"
        );
    }

    #[tokio::test]
    async fn a_pair_with_no_facts_to_disagree_about_never_reaches_the_model() {
        // The prefilter is the whole economic argument for this feature: a
        // model call is minutes, and most near pairs have nothing to judge.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(
            completer.calls(),
            0,
            "the prefilter let a factless pair through"
        );
        assert_eq!(out.judged, 0);
        assert_eq!(
            core.store
                .pairs_by_state(PairState::NoConflict, 10)
                .await
                .unwrap()
                .len(),
            1,
            "a cleared pair must leave the pending queue"
        );
    }

    #[tokio::test]
    async fn the_judge_stops_at_its_budget() {
        // One sweep must not be able to occupy the GPU for an hour.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.consolidate.max_judgements = 1;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"contradicts":false}"#.into(),
            r#"{"contradicts":false}"#.into(),
            r#"{"contradicts":false}"#.into(),
        ]));
        core.completer = completer.clone();
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.93, 0.37]),
                ("timeout is 90 seconds", [0.94, 0.34]),
            ],
        )
        .await;

        sweep_and_judge(&core).await;
        assert_eq!(completer.calls(), 1, "the budget was ignored");
    }

    #[tokio::test]
    async fn a_failed_judgement_leaves_the_pair_pending() {
        // A dead endpoint must not silently clear a queue of real conflicts.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            "not json".into(),
        ]));
        disagreeing(&core).await;

        run(&core).await.unwrap();
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
    async fn one_synthesis_call_emitting_a_passage_twice_resolves_itself() {
        // Same corpus, same call, one text wholly inside the other. That is a
        // defect in one artifact rather than two sources disagreeing, and it
        // sat on the review queue because it scores below auto_supersede.
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

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 1, "{out:?}");
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
        let a = seed(
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
    async fn a_pair_with_nothing_to_disagree_about_never_reaches_the_queue() {
        // The prefilter already knows these two state no differing value, but
        // it only ran when the judge was enabled — and the judge is off by
        // default. So every near pair became a question for a person, which is
        // a question with no answer to give.
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("Mount the filesystem before writing.", [1.0, 0.0]),
                ("Attach the volume before writing.", [0.93, 0.37]),
            ],
        )
        .await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 0, "{out:?}");
        assert_eq!(out.closed, 1, "{out:?}");
        assert!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .is_empty()
        );
        // Closing a question is not hiding an answer.
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
    async fn a_pair_stating_different_values_still_waits_for_a_person() {
        let core = test_core().await;
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 90 seconds", [0.93, 0.37]),
            ],
        )
        .await;
        let out = run(&core).await.unwrap();
        assert_eq!(out.queued, 1, "{out:?}");
    }

    #[tokio::test]
    async fn the_sweep_makes_no_inference_call_and_arms_one_unit_per_pair() {
        // Twenty judge calls in one job was the second-worst blocker in the
        // system after synthesis: a consolidation run held every capture behind
        // it for as long as twenty calls took. The sweep now decides which
        // pairs are worth asking about — all of it local — and arms a unit each.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"contradicts":false}"#.into(),
        ]));
        core.completer = completer.clone();
        disagreeing(&core).await;

        let out = run(&core).await.unwrap();

        assert_eq!(completer.calls(), 0, "the sweep called the model");
        assert_eq!(out.judged, 1, "no judge unit was armed: {out:?}");
        let armed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE stage = 'judge' AND state = 'pending'",
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
    async fn a_dead_endpoint_leaves_every_pair_pending() {
        // This used to assert that the sweep stopped judging after the first
        // transport failure, because the next nineteen calls would fail the
        // same way and each cost a full timeout. That guard still exists, but it
        // moved: the circuit breaker on the gate holds *all* background calls
        // after three consecutive transport failures, which covers embedding and
        // synthesis too rather than only this loop. It has its own test.
        //
        // What has to remain true here is the part that was never about calls:
        // a dead endpoint must not silently clear a queue of real conflicts.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.completer = completer.clone();
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.94, 0.342]),
                ("it listens on 8080", [-1.0, 0.0]),
                ("it listens on 9090", [-0.94, -0.342]),
            ],
        )
        .await;

        let out = sweep_and_judge(&core).await;
        assert_eq!(out.queued, 2, "not enough pairs to prove anything: {out:?}");
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            2,
            "a dead endpoint cleared pairs it never actually judged"
        );
    }

    #[tokio::test]
    async fn a_pair_the_model_keeps_failing_on_goes_to_the_back_of_the_queue() {
        // An unreadable reply leaves the pair pending on purpose. Ordered by
        // score alone, the same top-scoring pair would then absorb every
        // sweep's budget forever and the rest would never be judged at all.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.consolidate.max_judgements = 1;
        core.completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            "not json".into(),
            r#"{"contradicts":true,"detail":"30 versus 90"}"#.into(),
        ]));
        seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.999, 0.01]),
                ("timeout is 90 seconds", [0.9, 0.44]),
            ],
        )
        .await;

        sweep_and_judge(&core).await;
        sweep_and_judge(&core).await;

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
    async fn a_sweep_leaves_a_judge_unit_that_is_already_queued_alone() {
        // Two ways this went wrong at once. Re-arming a queued unit wound its
        // `attempts` back, so a pair the model will not judge never reached
        // `MAX_ATTEMPTS` and never reached the close-out that hands it to a
        // later sweep — forever young, exactly as the reconciliation sweep used
        // to keep windows. And `pairs_to_judge` orders by `judge_attempts`, so a
        // pair still waiting for its first call leads every sweep: the budget
        // went on re-arming it while pairs recorded since never got a turn.
        let mut core = test_core().await;
        core.consolidate.judge = true;
        core.consolidate.max_judgements = 1;
        // Two pairs in the review band and nothing near enough to cluster, so
        // the sweep has a second pair to reach once the first is spoken for.
        let ids = seed(
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
            sqlx::query_as("SELECT target_id, id FROM jobs WHERE stage = 'judge'")
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
        let armed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE stage = 'judge'")
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
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.examined, out.superseded, out.queued), (0, 0, 0));
    }
}
