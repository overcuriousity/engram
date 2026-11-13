//! What a generation scored while serving: a negative cannot be replayed, so the watch compares
//! the live generation's observations with its parent's, no re-ranking.

use crate::core::Core;
use crate::error::Result;

/// The lived record of one generation; `negatives` is a weighted sum, not a count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lived {
    pub positives: usize,
    pub negatives: f32,
    pub observations: usize,
}

impl Lived {
    fn rate(&self) -> Option<f64> {
        (self.observations > 0)
            .then(|| (self.positives as f64 - self.negatives as f64) / self.observations as f64)
    }
}

/// Everything still usable that was observed under `generation_id`.
pub async fn lived(core: &Core, generation_id: &str) -> Result<Lived> {
    let r = sqlx::query!(
        "SELECT COUNT(*) AS n,
                COALESCE(SUM(CASE WHEN strength > 0 THEN 1 ELSE 0 END), 0) AS positives,
                CAST(COALESCE(SUM(CASE WHEN strength < 0 THEN -strength ELSE 0 END), 0) AS REAL)
                    AS negatives
           FROM observations
          WHERE generation_id = ? AND excluded_at IS NULL",
        generation_id
    )
    .fetch_one(&core.store.pool)
    .await?;
    Ok(Lived {
        positives: r.positives as usize,
        negatives: r.negatives as f32,
        observations: r.n as usize,
    })
}

/// Observations the newer generation needs before a revert: the noise is `2/n` a side,
/// so ten puts it at a fifth of the range (as `rehearsed::MIN_PROBES`).
pub const MIN_OBSERVATIONS: usize = 10;

/// Whether the adopted generation keeps its place: unless the older rate beats it by more than
/// one flipped observation (`2/n` a side). Not corrected for repeated testing; reverts are cheap.
pub fn holds_up(new: &Lived, old: &Lived) -> bool {
    if new.observations < MIN_OBSERVATIONS {
        return true;
    }
    let (Some(new_rate), Some(old_rate)) = (new.rate(), old.rate()) else {
        return true;
    };
    let one_observation = 2.0 / new.observations as f64 + 2.0 / old.observations as f64;
    old_rate - new_rate <= one_observation
}

/// What ends a watch: the newer generation clears the gate against the older, or has
/// as many observations as the older had.
pub fn settled(new: &Lived, old: &Lived) -> bool {
    // Nothing on the older side ends the watch (`exclude_orphaned_observations` can zero it);
    // returning `false` here would hold the base under watch forever.
    let Some(old_rate) = old.rate() else {
        return true;
    };
    // Nothing on the newer side must keep waiting: a fresh adoption has no evidence yet.
    let Some(new_rate) = new.rate() else {
        return false;
    };
    let one_observation = 2.0 / new.observations as f64 + 2.0 / old.observations as f64;
    new_rate - old_rate > one_observation || new.observations >= old.observations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::generations::{GenerationParams, NewGeneration};
    use crate::store::observations::{NewObservation, Source};

    async fn base() -> (Core, String) {
        let core = crate::core::test_support::test_core().await;
        let first = core
            .store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.05,
                    per_source_cap: Some(3),
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        (core, first)
    }

    async fn another_generation(core: &Core, parent: &str) -> String {
        core.store
            .adopt_generation(
                &NewGeneration {
                    params: GenerationParams {
                        recency_weight: 0.1,
                        per_source_cap: Some(3),
                        ..Default::default()
                    },
                    embed_recipe: "recipe-a".into(),
                    chat_model: "qwen".into(),
                    parent_id: Some(parent.to_string()),
                },
                "run-1",
                0.04,
            )
            .await
            .unwrap()
            .expect("the parent is live")
    }

    async fn observe(core: &Core, generation: &str, source: Source) {
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.to_string(),
                query: "how did I mount it".into(),
                query_vec: vec![0.1, 0.2, 0.3],
                embed_model: "fake".into(),
                artifact_id: Some("art-1".into()),
                rank: Some(1),
                source,
                event_id: None,
            })
            .await
            .unwrap();
    }

    #[test]
    fn a_generation_that_earned_more_positives_holds_up() {
        let new = Lived {
            positives: 12,
            negatives: 1.0,
            observations: 13,
        };
        let old = Lived {
            positives: 6,
            negatives: 4.0,
            observations: 10,
        };
        assert!(holds_up(&new, &old));
    }

    #[test]
    fn a_generation_that_lost_ground_does_not_hold_up() {
        let new = Lived {
            positives: 4,
            negatives: 8.0,
            observations: 12,
        };
        let old = Lived {
            positives: 9,
            negatives: 1.0,
            observations: 10,
        };
        assert!(!holds_up(&new, &old));
    }

    #[test]
    fn a_tie_keeps_the_newer_generation() {
        let a = Lived {
            positives: 9,
            negatives: 3.0,
            observations: 12,
        };
        assert!(holds_up(&a, &a), "reverting is a change too");
    }

    #[test]
    fn too_few_observations_hold_up_rather_than_revert() {
        let old = Lived {
            positives: 40,
            negatives: 0.0,
            observations: 40,
        };
        let one = Lived {
            positives: 0,
            negatives: 1.0,
            observations: 1,
        };
        assert!(holds_up(&one, &old), "one observation decides nothing");
        // Nine neither: the arithmetic alone would revert on two give-ups against forty positives.
        let nine = Lived {
            positives: 0,
            negatives: 9.0,
            observations: 9,
        };
        assert!(holds_up(&nine, &old), "under the floor nothing is decided");
        let ten = Lived {
            positives: 0,
            negatives: 10.0,
            observations: 10,
        };
        assert!(!holds_up(&ten, &old), "and at the floor the record speaks");
    }

    #[test]
    fn a_generation_nobody_has_used_yet_holds_up() {
        let new = Lived {
            positives: 0,
            negatives: 0.0,
            observations: 0,
        };
        let old = Lived {
            positives: 40,
            negatives: 0.0,
            observations: 40,
        };
        assert!(holds_up(&new, &old));
        assert!(!settled(&new, &old), "and nothing is decided about it");
    }

    #[test]
    fn a_watch_whose_record_was_orphaned_ends_rather_than_wedging() {
        // `exclude_orphaned_observations` zeroes the parent; `false` here wedges the watch.
        let new = Lived {
            positives: 3,
            negatives: 1.0,
            observations: 4,
        };
        let orphaned = Lived {
            positives: 0,
            negatives: 0.0,
            observations: 0,
        };
        assert!(
            settled(&new, &orphaned),
            "nothing left to separate it from ends the watch"
        );
        assert!(
            holds_up(&new, &orphaned),
            "and the newer generation keeps its place, as a tie already decides"
        );
    }

    #[test]
    fn a_watch_ends_when_the_evidence_separates_or_matches_the_record() {
        let old = Lived {
            positives: 6,
            negatives: 4.0,
            observations: 10,
        };
        let thin = Lived {
            positives: 2,
            negatives: 0.0,
            observations: 2,
        };
        assert!(!settled(&thin, &old), "two observations decide nothing");
        let clearly_better = Lived {
            positives: 6,
            negatives: 0.0,
            observations: 6,
        };
        assert!(settled(&clearly_better, &old));
        let as_long_as_the_record = Lived {
            positives: 6,
            negatives: 4.0,
            observations: 10,
        };
        assert!(
            settled(&as_long_as_the_record, &old),
            "watched as long as it was measured against"
        );
    }

    #[tokio::test]
    async fn lived_counts_only_what_happened_under_that_generation() {
        let (core, first) = base().await;
        observe(&core, &first, Source::Cited).await;
        let second = another_generation(&core, &first).await;
        observe(&core, &second, Source::GaveUp).await;

        let first_lived = lived(&core, &first).await.unwrap();
        assert_eq!(first_lived.positives, 1);
        assert_eq!(first_lived.observations, 1);
        let second_lived = lived(&core, &second).await.unwrap();
        assert_eq!(second_lived.positives, 0);
        assert!(second_lived.negatives > 0.0);
        assert_eq!(second_lived.observations, 1);
    }

    #[tokio::test]
    async fn an_excluded_observation_is_not_part_of_the_record() {
        let (core, first) = base().await;
        observe(&core, &first, Source::Cited).await;
        let id = core
            .store
            .observations_for_generation(&first, 1)
            .await
            .unwrap()[0]
            .id
            .clone();
        core.store.exclude_observation(&id).await.unwrap();
        assert_eq!(lived(&core, &first).await.unwrap().observations, 0);
    }
}
