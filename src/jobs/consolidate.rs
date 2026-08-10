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
use crate::store::artifacts::Chunk;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Outcome {
    pub examined: usize,
    pub superseded: usize,
    pub queued: usize,
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
fn keeper(members: &[Chunk]) -> &Chunk {
    members
        .iter()
        .max_by_key(|c| (c.created_at, c.id.as_str()))
        .expect("a cluster has at least one member")
}

pub async fn run(core: &Core) -> Result<Outcome> {
    let cfg = &core.consolidate;
    if !cfg.enabled {
        return Ok(Outcome::default());
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
        if c.superseded_by.is_some() {
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
            // no row behind it is a hidden artifact nothing can explain.
            core.store.set_superseded_by(&c.id, Some(&keep.id)).await?;
            core.vectors.set_superseded(&c.id, true).await?;
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
        if a.superseded_by.is_some() || b.superseded_by.is_some() {
            continue;
        }
        if core.store.record_pair(&p.a, &p.b, p.score).await? {
            out.queued += 1;
            tracing::info!(a = %p.a, b = %p.b, score = p.score, "queued a pair for review");
        }
    }

    if cfg.judge {
        let (judged, contradictions) = judge_pending(core).await?;
        out.judged = judged;
        out.contradictions = contradictions;
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
async fn judge_pending(core: &Core) -> Result<(usize, usize)> {
    let pending = core
        .store
        .pairs_by_state(crate::store::pairs::PairState::Pending, 200)
        .await?;

    let (mut judged, mut contradictions) = (0usize, 0usize);
    for p in pending {
        if judged >= core.consolidate.max_judgements {
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

        // The whole economic argument: most near pairs have no value in common
        // to disagree about, and a model call is minutes on this hardware.
        if !crate::infer::facts::may_disagree(&a.text, &b.text) {
            core.store
                .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, None)
                .await?;
            continue;
        }

        judged += 1;
        let reply = match core
            .completer
            .complete(
                crate::infer::prompt::JUDGE_SYSTEM,
                &crate::infer::prompt::judge_prompt(&a.text, &b.text),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(pair = p.id, error = %e, "judge call failed; pair stays pending");
                continue;
            }
        };

        match crate::infer::prompt::parse_judgement(&reply) {
            Ok((true, detail)) => {
                contradictions += 1;
                core.store
                    .set_pair_state(
                        p.id,
                        crate::store::pairs::PairState::Contradiction,
                        detail.as_deref(),
                    )
                    .await?;
                tracing::info!(pair = p.id, a = %a.id, b = %b.id, "artifacts disagree");
            }
            Ok((false, _)) => {
                core.store
                    .set_pair_state(p.id, crate::store::pairs::PairState::NoConflict, None)
                    .await?;
            }
            Err(e) => {
                tracing::warn!(pair = p.id, error = %e, "judge reply unreadable; pair stays pending");
            }
        }
    }
    Ok((judged, contradictions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::test_support::test_core;
    use crate::store::artifacts::NewArtifact;
    use crate::store::pairs::PairState;
    use crate::vector::{VectorPayload, VectorPoint};

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
                    superseded: None,
                },
            })
            .collect();
        core.vectors.upsert(points).await.unwrap();
        made.into_iter().map(|c| c.id).collect()
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
    async fn a_pair_in_the_review_band_is_queued_not_superseded() {
        // 0.88 is where two genuinely distinct artifacts about one subsystem
        // routinely sit. Acting on that score destroys knowledge.
        let core = test_core().await;
        let ids = seed(&core, &[("first", [1.0, 0.0]), ("second", [0.93, 0.37])]).await;

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

        let out = run(&core).await.unwrap();
        assert_eq!((out.judged, out.contradictions), (1, 1), "{out:?}");
        let found = core
            .store
            .pairs_by_state(PairState::Contradiction, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].detail.as_deref(), Some("1.21.4 versus 1.30.0"));
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

        run(&core).await.unwrap();
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
    async fn the_sweep_is_off_when_configuration_says_so() {
        let mut core = test_core().await;
        core.consolidate.enabled = false;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.examined, out.superseded, out.queued), (0, 0, 0));
    }
}
