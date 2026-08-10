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

    if out.superseded > 0 || out.queued > 0 {
        tracing::info!(
            examined = out.examined,
            superseded = out.superseded,
            queued = out.queued,
            "consolidation sweep finished"
        );
    }
    Ok(out)
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

    #[tokio::test]
    async fn the_sweep_is_off_when_configuration_says_so() {
        let mut core = test_core().await;
        core.consolidate.enabled = false;
        seed(&core, &[("first", [1.0, 0.0]), ("second", [0.9999, 0.01])]).await;
        let out = run(&core).await.unwrap();
        assert_eq!((out.examined, out.superseded, out.queued), (0, 0, 0));
    }
}
