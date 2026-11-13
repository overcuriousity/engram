//! Consolidation: the backstop for `jobs::relate`'s arming, clustering, dedupe pacing and repairs.
//! A score alone never hides anything: embeddings barely distinguish negation.

use crate::core::Core;
use crate::error::{Error, Result};
use crate::store::artifacts::Chunk;
use crate::store::jobs::Stage;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Outcome {
    pub superseded: usize,
    /// Armed, not answered: the calls happen later, one unit at a time.
    pub judged: usize,
    pub reopened: u64,
    pub repaired: u64,
    pub staled: u64,
    /// Counted because `jobs::did_work` reads only this report; uncounted, a repair-only tick backs
    /// off.
    pub reaped: usize,
}

/// Disjoint-set over artifact ids, so every member of a cluster points at the one survivor
/// rather than at an artifact that is itself hidden.
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

/// Newest by capture time, id as tie-break: the most re-captured thing is a document since updated.
fn keeper(members: &[Chunk]) -> Option<&Chunk> {
    members.iter().max_by_key(|c| (c.created_at, c.id.as_str()))
}

/// `known` is false when a member could not be read; closing on that would settle a live pair for
/// good.
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
        // Both still in results: a supersede failed and warned; the pair stays filed.
        (true, true) => None,
        (true, false) => Some(format!("near-identical; {a_id} kept")),
        (false, true) => Some(format!("near-identical; {b_id} kept")),
        (false, false) => Some("near-identical; neither side is in results any more".to_string()),
    }
}

/// `claim_job` orders by `(state, attempts, seq, id)`, so this puts hygiene behind every capture
/// window.
const DEDUPE_SEQ_BASE: i64 = 1_000_000;

pub(crate) fn lifecycle_row_of(c: &Chunk) -> crate::vector::LifecycleRow {
    crate::vector::LifecycleRow {
        artifact_id: c.id.clone(),
        status: c.status,
        superseded_by: c.superseded_by.clone(),
        last_verified_at: c.last_verified_at.unwrap_or(c.created_at),
    }
}

/// The list is almost always empty; the cap bounds a sweep after a long outage.
const DRIFT_REPAIR_BATCH: usize = 5_000;

/// Make the vector store's lifecycle payloads agree with SQLite, the source of truth. Reads
/// `lifecycle_dirty`, which only holds while every payload writer (including `jobs::embed`) marks
/// it. Returns how many it rewrote: a repair that fires on a base in agreement is a bug.
pub(crate) async fn repair_lifecycle_drift(core: &Core) -> Result<usize> {
    // Under the lifecycle lock, or a payload-first reveal can interleave and hide an artifact
    // unmarked.
    let _guard = core.lifecycle_lock.lock().await;
    let dirty = core
        .store
        .dirty_lifecycle_artifacts(DRIFT_REPAIR_BATCH)
        .await?;
    if dirty.is_empty() {
        return Ok(0);
    }
    // A reaped row's fix is finishing the point delete, not a payload write.
    let (reaped, standing): (Vec<_>, Vec<_>) = dirty.iter().partition(|c| c.reaped_at.is_some());
    if !reaped.is_empty() {
        let ids: Vec<String> = reaped.iter().map(|c| c.id.clone()).collect();
        core.vectors.delete_artifacts(&ids).await?;
    }
    let rows: Vec<crate::vector::LifecycleRow> =
        standing.iter().map(|c| lifecycle_row_of(c)).collect();
    if !rows.is_empty() {
        core.vectors.apply_lifecycle(&rows).await?;
    }
    // Only after the vector writes return, or a failed write becomes permanent drift.
    let ids: Vec<String> = dirty.iter().map(|c| c.id.clone()).collect();
    core.store.clear_lifecycle_dirty(&ids).await?;
    tracing::info!(
        repaired = ids.len(),
        "finished lifecycle writes that never reached the vector store"
    );
    Ok(ids.len())
}

pub async fn run(core: &Core) -> Result<Outcome> {
    let cfg = &core.consolidate;
    if !cfg.enabled {
        return Ok(Outcome::default());
    }

    // Finish ingest before judging duplicates; the repair ticker is what guarantees this runs.
    crate::jobs::reconcile::run(core).await?;

    // No repair may take the sweep with it. Only merge repair is here: merges exist only because
    // the sweep made them. An interrupted merge shows only in a join across the lineage.
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
    // A merge that will never be embedded: its only signal was a forever-retrying job.
    let mut out = Outcome::default();
    match core.store.stranded_merges(50).await {
        Ok(stranded) => {
            for id in stranded {
                if let Err(e) = crate::jobs::merge::reap_stranded(core, &id).await {
                    tracing::warn!(merged = %id, error = %e, "could not reap a stranded merge");
                } else {
                    out.reaped += 1;
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not look for stranded merges"),
    }
    // A lost source is a provenance claim the merge can no longer support, not data loss.
    if let Err(e) = crate::jobs::merge::flag_orphans(core).await {
        tracing::warn!(error = %e, "could not flag merged artifacts that lost a source");
    }

    // Pairs the old fan-in cap refused have `judge_attempts = 0`, so reopening redoes no work.
    match core.store.reopen_oversized().await {
        Ok(0) => {}
        Ok(n) => {
            out.reopened = n;
            tracing::info!(pairs = n, "reopened pairs the fan-in cap had refused");
        }
        Err(e) => tracing::warn!(error = %e, "could not reopen the pairs the cap refused"),
    }

    // Before anything is judged: a pair naming a hidden artifact is unanswerable.
    match follow_supersessions(core).await {
        Ok(f) if f.settled() == 0 => {}
        Ok(f) => {
            out.repaired = f.moved;
            out.staled += f.staled;
            tracing::info!(
                moved = f.moved,
                staled = f.staled,
                "settled the pairs a supersession had left behind"
            );
        }
        Err(e) => tracing::warn!(error = %e, "could not move the pairs a supersession left behind"),
    }

    // The review queue neither shows nor counts these, so they pile up out of reach. Reversible
    // (`Core::reactivate`), and after the move above, which is the better answer.
    match core.store.stale_unreachable_pairs().await {
        Ok(0) => {}
        Ok(n) => {
            out.staled += n;
            tracing::info!(pairs = n, "took pairs nobody can act on off the queue");
        }
        Err(e) => tracing::warn!(error = %e, "could not settle the pairs nobody can act on"),
    }

    // Group first, then decide: resolving pairwise leaves A pointing at a hidden B.
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
    // Unreadable is not gone: a briefly unwell store must not settle a pair.
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
        let Some(keep) = keeper(group) else {
            continue;
        };
        for c in group.iter().filter(|c| c.id != keep.id) {
            if crate::jobs::try_supersede(core, &c.id, &keep.id, "a near-identical artifact").await
            {
                hidden.insert(c.id.clone());
                out.superseded += 1;
            }
        }
    }

    // Close answered rows, or the 500-row window fills and new pairs never enter it.
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
                crate::store::pairs::DecidedBy::Model,
            )
            .await
        {
            tracing::warn!(pair = p.id, error = %e, "could not close a settled near-identical pair");
        }
    }

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

/// The repair half of `Store::follow_supersession`, for crashes, direct `set_superseded_by` writers
/// and older rows. Bounded per sweep.
async fn follow_supersessions(core: &Core) -> Result<crate::store::pairs::Followed> {
    let mut out = crate::store::pairs::Followed::default();
    for (loser, winner) in core.store.supersessions_with_open_pairs(200).await? {
        let f = core.store.follow_supersession(&loser, &winner).await?;
        out.moved += f.moved;
        // Counted: `jobs::did_work` reads only this report.
        out.staled += f.staled;
    }
    Ok(out)
}

/// `max_dedupe_per_tick` bounds what one tick arms, not units in flight; `live_job` arms a pair
/// once.
pub(crate) async fn arm_dedupe(core: &Core) -> Result<usize> {
    // Zero is the off switch for the model; `reconcile` drains older bases regardless.
    if core.consolidate.max_dedupe_per_tick == 0 {
        return Ok(0);
    }
    // Arming nothing when the budget is spent is what lets the empty-run backoff engage.
    if !core.may_act(crate::store::actions::Job::Dedupe).await? {
        tracing::info!("budget spent; no pairs armed until the window moves");
        return Ok(0);
    }
    // Asked pairs lead and are often `Duplicate`, not `Pending`; this re-arms an ask whose first
    // run wrote nothing. `pairs_awaiting_synthesis` excludes merged and unparseable pairs, so it
    // cannot loop.
    let asked = core.store.pairs_awaiting_synthesis(200).await?;
    let pending = core.store.pairs_to_judge(200).await?;
    // An asked `Pending` pair is in both lists; dedupe so it takes one budget slot.
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let queue: Vec<_> = asked
        .into_iter()
        .chain(pending)
        .filter(|p| seen.insert(p.id))
        .collect();

    let mut armed = 0usize;
    for p in queue {
        if armed >= core.consolidate.max_dedupe_per_tick {
            tracing::info!(
                budget = core.consolidate.max_dedupe_per_tick,
                "dedupe budget spent; the rest wait for the next tick"
            );
            break;
        }
        // A cascaded-away member is skipped: its `NotFound` would read as the sweep's target being
        // gone.
        let (a_live, b_live) = match (
            core.store.artifact_in_results(&p.a_id).await?,
            core.store.artifact_in_results(&p.b_id).await?,
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => continue,
        };

        // `Core::supersede` would refuse a verdict on a retired member. The unit checks again.
        if !a_live || !b_live {
            // The card reads a standing flag as "the writing is queued".
            if p.synthesis_asked {
                core.store.clear_pair_synthesis(p.id).await?;
            }
            core.store
                .set_pair_state(
                    p.id,
                    crate::store::pairs::PairState::Dismissed,
                    None,
                    crate::store::pairs::DecidedBy::Model,
                )
                .await?;
            continue;
        }

        // Already queued; with no attempts it would otherwise lead every tick.
        let target = p.id.to_string();
        if core.store.live_job(Stage::Dedupe, &target).await? {
            continue;
        }

        // Idle-only: re-arming resets `attempts`, so a pair the model will not judge never closes
        // out.
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

    /// The sweep only arms units; tests about what the judge said drive the queue too.
    async fn sweep_and_dedupe(core: &crate::core::Core) -> Outcome {
        let out = run(core).await.unwrap();
        for _ in 0..100 {
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            if !crate::jobs::run_one(core).await.unwrap_or(false) {
                break;
            }
        }
        out
    }

    /// `title` is the subject the dedupe prompt is built around.
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
                title: title.map(str::to_string),
                ..Default::default()
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
                    created_at: c.created_at,
                    ..Default::default()
                },
            })
            .collect();
        core.vectors.upsert(points).await.unwrap();
        made.into_iter().map(|c| c.id).collect()
    }

    /// `seed`, then each artifact relates as indexing does live.
    async fn seed_related(core: &crate::core::Core, vectors: &[(&str, [f32; 2])]) -> Vec<String> {
        let ids = seed(core, vectors).await;
        for id in &ids {
            crate::jobs::relate::run(core, id).await.unwrap();
        }
        ids
    }

    async fn leave_pair(
        core: &crate::core::Core,
        ids: &[String],
        (a, b): (usize, usize),
        state: PairState,
        detail: Option<&str>,
    ) -> i64 {
        use crate::store::pairs::DecidedBy;
        core.store
            .record_pair(&ids[a], &ids[b], 0.91)
            .await
            .unwrap();
        let id = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.a_id == ids[a] || p.b_id == ids[a])
            .unwrap()
            .id;
        core.store
            .set_pair_state(id, state, detail, DecidedBy::Model)
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn an_old_base_has_nothing_waiting_after_two_sweeps() {
        let core = test_core().await;
        let ids = seed_titled(
            &core,
            &[
                ("A", "alpha", [1.0, 0.0]),
                ("B", "beta", [0.99, 0.1]),
                ("C", "gamma", [0.0, 1.0]),
                ("D", "delta", [0.1, 0.99]),
                ("E", "eps", [0.7, 0.7]),
                ("F", "zeta", [0.71, 0.69]),
                ("G", "eta", [0.5, 0.86]),
                ("H", "theta", [0.52, 0.85]),
                ("I", "iota", [-1.0, 0.0]),
                ("J", "kappa", [-0.99, 0.1]),
                ("K", "lambda", [0.0, -1.0]),
                ("L", "mu", [0.1, -0.99]),
            ],
        )
        .await;
        let vacuous = leave_pair(&core, &ids, (8, 9), PairState::Vacuous, Some("empty")).await;
        let refused = leave_pair(
            &core,
            &ids,
            (10, 11),
            PairState::Contradiction,
            Some(
                "These two state a value differently, and merging them would have dropped \
                 one of them. Which is current is the judgement this hands over.",
            ),
        )
        .await;
        let dup = leave_pair(
            &core,
            &ids,
            (0, 1),
            PairState::Duplicate,
            Some("same thing"),
        )
        .await;
        let unmergeable = leave_pair(&core, &ids, (2, 3), PairState::Unmergeable, Some("x")).await;
        let taken_back = leave_pair(
            &core,
            &ids,
            (4, 5),
            PairState::Contradiction,
            Some(crate::jobs::dedupe::TAKEN_BACK_OLD),
        )
        .await;
        let asked = leave_pair(
            &core,
            &ids,
            (6, 7),
            PairState::Duplicate,
            Some("same thing"),
        )
        .await;
        assert!(core.store.ask_pair_synthesis(asked).await.unwrap());

        let first = core.store.drain_waiting_pairs().await.unwrap();
        let second = core.store.drain_waiting_pairs().await.unwrap();

        assert_eq!(first.rejudged, 2);
        assert_eq!(first.closed, 3);
        assert_eq!((second.closed, second.rejudged), (0, 0), "idempotent");
        let d = core.store.get_pair(dup).await.unwrap();
        assert_eq!(d.state, PairState::Pending, "the judge reads it again");
        assert!(!d.synthesis_asked);
        assert_eq!(
            core.store.get_pair(vacuous).await.unwrap().state,
            PairState::Pending,
            "a vacuous recommendation is judged again, not left for a press"
        );
        for id in [unmergeable, taken_back, refused] {
            assert_eq!(
                core.store.get_pair(id).await.unwrap().state,
                PairState::NoConflict
            );
        }
        let a = core.store.get_pair(asked).await.unwrap();
        assert_eq!(a.state, PairState::Duplicate, "an operator's ask stands");
        assert!(a.synthesis_asked);
    }

    #[tokio::test]
    async fn the_drain_runs_with_the_dedupe_model_off() {
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 0;
        core.consolidate.enabled = false;
        let ids = seed_titled(
            &core,
            &[("A", "alpha", [1.0, 0.0]), ("B", "beta", [0.99, 0.1])],
        )
        .await;
        let id = leave_pair(&core, &ids, (0, 1), PairState::Vacuous, Some("empty")).await;

        assert_eq!(arm_dedupe(&core).await.unwrap(), 0);
        run(&core).await.unwrap();
        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Vacuous
        );

        crate::jobs::reconcile::run(&core).await.unwrap();

        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Pending
        );
    }

    #[tokio::test]
    async fn a_real_disagreement_is_left_for_search_to_show() {
        let core = test_core().await;
        let ids = seed_titled(
            &core,
            &[("A", "alpha", [1.0, 0.0]), ("B", "beta", [0.99, 0.1])],
        )
        .await;
        let id = leave_pair(
            &core,
            &ids,
            (0, 1),
            PairState::Contradiction,
            Some("A says port 80, B says port 8080."),
        )
        .await;

        let drained = core.store.drain_waiting_pairs().await.unwrap();

        assert_eq!((drained.closed, drained.rejudged), (0, 0));
        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Contradiction
        );
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
    async fn a_near_identical_pair_goes_to_the_judge_first_and_hides_nothing() {
        // Embeddings barely distinguish negation, so a score alone must hide nothing.
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;

        let out = run(&core).await.unwrap();
        assert_eq!(out.superseded, 0, "{out:?}");
        assert!(out.judged >= 1, "{out:?}");
        for id in &ids {
            assert!(core.store.get_artifact(id).await.unwrap().in_results());
        }
        let hits = core
            .vectors
            .search(&[1.0, 0.0], &Default::default(), 10, &Default::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn reactivating_a_superseded_artifact_survives_the_next_sweep() {
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
    async fn the_sweep_moves_a_pair_a_supersession_left_behind() {
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.0, 1.0]),
                ("the timeout is 30s", [0.999, 0.01]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.8).await.unwrap();
        let id = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap()[0]
            .id;
        core.store
            .set_pair_state(
                id,
                PairState::Contradiction,
                Some("30 seconds vs 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        // Behind the write path's back, as a crash between its two writes leaves it.
        core.store
            .set_superseded_by(&ids[0], Some(&ids[2]))
            .await
            .unwrap();

        run(&core).await.unwrap();

        let open = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert!(
            open[0].a_id == ids[2] || open[0].b_id == ids[2],
            "the verdict still names the artifact that is out of results"
        );
    }

    #[tokio::test]
    async fn undoing_a_supersession_puts_the_question_back() {
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 30s", [0.999, 0.01]),
            ],
        )
        .await;
        core.store
            .record_pair(&ids[0], &ids[1], 0.99)
            .await
            .unwrap();
        let id = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
            .id;

        core.supersede(&ids[0], &ids[1]).await.unwrap();
        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Stale,
            "the supersession answered it only for as long as it stands"
        );

        core.unsupersede(&ids[0]).await.unwrap();

        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Pending,
            "the artifact came back and its question did not"
        );
    }

    #[tokio::test]
    async fn the_sweep_takes_a_verdict_nobody_can_act_on_off_the_queue() {
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.0, 1.0]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.8).await.unwrap();
        let id = core
            .store
            .pair_between(&ids[0], &ids[1])
            .await
            .unwrap()
            .unwrap()
            .id;
        core.store
            .set_pair_state(
                id,
                PairState::Contradiction,
                Some("30 seconds vs 90"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        core.deprecate(&ids[1]).await.unwrap();

        run(&core).await.unwrap();

        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Stale
        );

        core.reactivate(&ids[1]).await.unwrap();
        assert_eq!(
            core.store.get_pair(id).await.unwrap().state,
            PairState::Pending,
            "deprecation is the reversible way out of results, except in the queue"
        );
    }

    #[tokio::test]
    async fn superseding_an_artifact_moves_its_open_pairs_onto_the_winner() {
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed(
            &core,
            &[
                ("the timeout is 30 seconds", [1.0, 0.0]),
                ("the timeout is 90 seconds", [0.9, 0.4]),
                ("the timeout is 30s", [0.999, 0.01]),
            ],
        )
        .await;
        core.store.record_pair(&ids[0], &ids[1], 0.8).await.unwrap();

        core.supersede(&ids[0], &ids[2]).await.unwrap();

        let open = core
            .store
            .pairs_by_state(PairState::Pending, 10)
            .await
            .unwrap();
        assert_eq!(open.len(), 1, "the pair was dropped instead of moved");
        assert!(
            open[0].a_id == ids[2] || open[0].b_id == ids[2],
            "the pair still names the artifact that was just hidden"
        );
    }

    #[tokio::test]
    async fn the_repair_pass_finishes_a_supersession_whose_payload_write_was_lost() {
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
        // `set_superseded_by` writes `status = 'superseded'` unconditionally.
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
        // `superseded_by` has no foreign key behind it.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        // Filed by hand: an older base's row, which the cluster pass still acts on.
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
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
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.judge = Some(completer.clone());
        // Judge off, so the only call this can count is the second sweep's.
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
        // `Core::supersede` refuses a deprecated side, which never sets `superseded_by`.
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![]));
        core.judge = Some(completer.clone());
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
    async fn a_reaped_row_whose_point_survived_is_repaired_by_deletion() {
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        core.store
            .set_artifact_status(&ids[0], ArtifactStatus::Deprecated)
            .await
            .unwrap();
        // Negative age floor: the row was retired this second and `bury` re-checks the predicate.
        core.store
            .bury(
                &ids[0],
                "{}",
                -60,
                None,
                None,
                &crate::jobs::reap::test_support::row(&ids[0]),
            )
            .await
            .unwrap();
        core.store.mark_lifecycle_dirty(&ids[0]).await.unwrap();

        assert_eq!(repair_lifecycle_drift(&core).await.unwrap(), 1);
        assert!(
            core.vectors
                .payloads_of(std::slice::from_ref(&ids[0]))
                .await
                .unwrap()
                .is_empty(),
            "the standing point must be deleted, not rewritten"
        );
        assert!(
            core.store
                .dirty_lifecycle_artifacts(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker must clear with the delete"
        );
    }

    #[tokio::test]
    async fn the_marker_repair_rewrites_nothing_when_the_two_stores_agree() {
        // Both scans are capped independently, so missing from the other list is not drift.
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

        assert_eq!(
            repair_lifecycle_drift(&core).await.unwrap(),
            0,
            "the repair fired on a base that agrees with itself"
        );
    }

    #[tokio::test]
    async fn a_lifecycle_write_that_lost_its_payload_is_repaired_from_the_marker() {
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;

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
        // 0.88 is where distinct artifacts about one subsystem sit; differing values keep it
        // queued.
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
        // Orthogonal vectors, so nothing else is paired.
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
        // `live_job` skips a pair whose unit is already queued.
        assert_eq!(arm_dedupe(&core).await.unwrap(), 1);
        assert_eq!(
            arm_dedupe(&core).await.unwrap(),
            0,
            "a pair with a live unit was armed again"
        );
    }

    #[tokio::test]
    async fn a_zero_budget_asks_nothing_and_still_records_everything() {
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
        // `pairs_by_state(NearIdentical, 500)` is ordered by score, so unclosed rows crowd new
        // pairs out.
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
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        core.store
            .record_settled_pair(&ids[0], &ids[1], 0.999, PairState::NearIdentical)
            .await
            .unwrap();
        core.vectors.delete_artifacts(&ids).await.unwrap();
        // Deprecated, so `supersede` refuses both directions.
        core.deprecate(&ids[0]).await.unwrap();
        core.deprecate(&ids[1]).await.unwrap();

        run(&core).await.unwrap();

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
        let core = test_core().await;
        seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        run(&core).await.unwrap();
        let second = run(&core).await.unwrap();
        assert_eq!(second.superseded, 0, "{second:?}");
    }

    #[tokio::test]
    async fn an_artifact_is_never_superseded_twice() {
        // A chain of supersessions is one the UI cannot resolve.
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
        for (a, b) in [(0, 1), (1, 2), (0, 2)] {
            core.store
                .record_settled_pair(&ids[a], &ids[b], 0.999, PairState::NearIdentical)
                .await
                .unwrap();
        }

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
        // Two artifacts saying the same thing in different words are merge candidates, not settled.
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
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        core.judge = Some(completer.clone());
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
        // Without a record of the first decision, the next relate would overrule a Restore.
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
        // Queued is not hidden: nothing changes until a dedupe unit rules.
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
        let mut core = test_core().await;
        let completer = std::sync::Arc::new(crate::infer::fake::ScriptedCompleter::new(vec![
            r#"{"relation":"distinct","detail":"different subjects"}"#.into(),
        ]));
        core.judge = Some(completer.clone());
        disagreeing(&core).await;

        let out = run(&core).await.unwrap();

        assert_eq!(completer.calls(), 0, "the sweep called the model");
        assert_eq!(out.judged, 1, "no judge unit was armed: {out:?}");
        let armed: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE stage = 'dedupe' AND state = 'pending'"
        )
        .fetch_one(&core.store.control.pool)
        .await
        .unwrap();
        assert_eq!(armed, 1);

        while crate::jobs::run_one(&core).await.unwrap() {}
        assert_eq!(completer.calls(), 1);
    }

    #[tokio::test]
    async fn a_pair_the_model_keeps_failing_on_goes_to_the_back_of_the_queue() {
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
        core.judge = Some(std::sync::Arc::new(
            crate::infer::fake::ScriptedCompleter::new(vec!["not json".into()]),
        ));
        let ids = seed(
            &core,
            &[
                ("timeout is 30 seconds", [1.0, 0.0]),
                ("timeout is 60 seconds", [0.999, 0.01]),
                ("timeout is 90 seconds", [0.9, 0.44]),
            ],
        )
        .await;
        // Same window, so 30 and 60 are not a pair: 30–90 (0.898) and 60–90 (0.903).
        for id in &ids[..2] {
            sqlx::query!("UPDATE artifacts SET segment_idx = 0 WHERE id = ?", id)
                .execute(&core.store.pool)
                .await
                .unwrap();
        }
        for id in &ids {
            crate::jobs::relate::run(&core, id).await.unwrap();
        }

        // Other units (the sweep's own housekeeping) may sit ahead of the dedupe unit.
        run(&core).await.unwrap();
        for _ in 0..20 {
            let asked = core
                .store
                .pairs_to_judge(10)
                .await
                .unwrap()
                .iter()
                .any(|p| p.judge_attempts > 0);
            if asked {
                break;
            }
            sqlx::query!("UPDATE jobs SET run_after = 0")
                .execute(&core.store.control.pool)
                .await
                .unwrap();
            assert!(crate::jobs::run_one(&core).await.unwrap(), "queue ran dry");
        }

        let pending = core.store.pairs_to_judge(10).await.unwrap();
        assert_eq!(pending.len(), 2, "{pending:?}");
        assert_eq!(pending[0].judge_attempts, 0, "{pending:?}");
        assert_eq!(pending[1].judge_attempts, 1, "{pending:?}");
        assert!(pending[0].score < pending[1].score, "{pending:?}");
    }
    #[tokio::test]
    async fn a_pair_whose_artifact_vanished_does_not_abandon_the_rest_of_the_sweep() {
        // A cascade can remove a pair between the snapshot and the read of its members.
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
        // The higher score leads, so the survivor is reached only if the missing pair did not end
        // the loop.
        core.store
            .record_pair(&ids[0], &ids[1], 0.95)
            .await
            .unwrap();
        core.store
            .record_pair(&ids[2], &ids[3], 0.90)
            .await
            .unwrap();

        // The cascade would remove the pair row too; the loop holds a pair read before the delete.
        sqlx::query!("PRAGMA foreign_keys = OFF")
            .execute(&core.store.pool)
            .await
            .unwrap();
        sqlx::query!("DELETE FROM artifacts WHERE id = ?", &ids[0])
            .execute(&core.store.pool)
            .await
            .unwrap();

        let armed = arm_dedupe(&core).await.unwrap();

        assert_eq!(
            armed, 1,
            "the pair that lost the race took the whole sweep down with it"
        );
        let target: String =
            sqlx::query_scalar!("SELECT target_id FROM jobs WHERE stage = 'dedupe'")
                .fetch_one(&core.store.control.pool)
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
        let mut core = test_core().await;
        core.consolidate.max_dedupe_per_tick = 1;
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

        // Nothing runs it: the worker is busy.
        run(&core).await.unwrap();
        let first: (String, i64) =
            sqlx::query!("SELECT target_id, id FROM jobs WHERE stage = 'dedupe'")
                .fetch_one(&core.store.control.pool)
                .await
                .map(|r| (r.target_id, r.id))
                .unwrap();
        let later = crate::store::now() + 3600;
        sqlx::query!(
            "UPDATE jobs SET attempts = 2, run_after = ? WHERE id = ?",
            later,
            first.1
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        run(&core).await.unwrap();

        let (attempts, run_after): (i64, i64) =
            sqlx::query!("SELECT attempts, run_after FROM jobs WHERE id = ?", first.1)
                .fetch_one(&core.store.control.pool)
                .await
                .map(|r| (r.attempts, r.run_after))
                .unwrap();
        assert_eq!(
            (attempts, run_after),
            (2, later),
            "the sweep wound a queued judge unit back to zero attempts"
        );
        let armed: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE stage = 'dedupe'")
            .fetch_one(&core.store.control.pool)
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

    #[tokio::test]
    async fn a_spent_day_arms_no_pairs_rather_than_arming_the_same_ones_for_ever() {
        let mut core = test_core().await;
        core.evolve.autonomous = crate::config::Autonomy::Full;
        core.evolve.max_actions_per_day = 0;
        disagreeing(&core).await;
        assert_eq!(arm_dedupe(&core).await.unwrap(), 0, "nothing is armed");
        assert_eq!(
            core.store
                .pairs_by_state(PairState::Pending, 10)
                .await
                .unwrap()
                .len(),
            1,
            "and the pair waits, exactly as it was"
        );
    }

    #[test]
    fn an_unreadable_member_leaves_the_filed_pair_alone() {
        // `record_settled_pair` only updates pending rows, so a wrong close is permanent.
        assert_eq!(close_filed_pair(false, true, false, true, "a", "b"), None);
        assert_eq!(close_filed_pair(true, false, true, false, "a", "b"), None);
        assert_eq!(close_filed_pair(true, true, true, true, "a", "b"), None);
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

    // Its roots are already behind it; retiring it would hide them behind a row nothing restores.
    #[tokio::test]
    async fn a_condensed_merge_whose_new_embed_fails_keeps_what_it_replaced() {
        use crate::store::artifacts::NewMerged;
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let m = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "both".into(),
                    ..Default::default()
                },
                &[ids[0].clone(), ids[1].clone()],
            )
            .await
            .unwrap();
        for id in &ids {
            core.store.set_superseded_by(id, Some(&m.id)).await.unwrap();
        }
        core.store
            .enqueue(crate::store::jobs::Stage::Embed, "artifact", &m.id)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET attempts = ? WHERE target_id = ?",
            crate::store::jobs::MAX_ATTEMPTS,
            &m.id
        )
        .execute(&core.store.control.pool)
        .await
        .unwrap();

        run(&core).await.unwrap();

        assert_eq!(
            core.store.get_artifact(&m.id).await.unwrap().status,
            ArtifactStatus::Active,
            "a merge that had landed was retired as though it never had"
        );
        for id in &ids {
            assert_eq!(
                core.store
                    .get_artifact(id)
                    .await
                    .unwrap()
                    .superseded_by
                    .as_deref(),
                Some(m.id.as_str()),
                "what it replaced is still behind it"
            );
        }
    }

    #[tokio::test]
    async fn a_merge_that_can_never_embed_is_reaped_and_its_pairs_left_with_both_sides() {
        use crate::store::artifacts::NewMerged;
        use crate::store::pairs::PairState;
        let core = test_core().await;
        let ids = seed_related(&core, &[("first", [1.0, 0.0]), ("second", [0.0, 1.0])]).await;
        let m = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "both".into(),
                    ..Default::default()
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
            .set_pair_merged(
                pid,
                &m.id,
                Some("same claim"),
                crate::store::pairs::DecidedBy::Model,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE jobs SET attempts = ? WHERE target_id = ?",
            crate::store::jobs::MAX_ATTEMPTS,
            &m.id
        )
        .execute(&core.store.control.pool)
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
            PairState::NoConflict,
            "the reaped merge's pair leaves both sides as they are"
        );
        for id in &ids {
            assert_eq!(
                core.store.get_artifact(id).await.unwrap().status,
                ArtifactStatus::Active
            );
        }

        let m2 = core
            .store
            .insert_merged_artifact(
                &NewMerged {
                    text: "x".into(),
                    ..Default::default()
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
                crate::store::pairs::DecidedBy::Model,
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
