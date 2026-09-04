//! What a generation scored while it was actually serving.
//!
//! The other half of the loop, and the half that cannot be counterfactual. A
//! candidate is adopted on a replay: positive observations re-ranked under
//! other settings, asking where *this* configuration would have put the thing
//! that mattered. A negative cannot be replayed that way — a give-up says this
//! list did not answer, and whether some other list would have is unknowable,
//! because that list was never shown to anybody.
//!
//! So the watch reads what happened instead: under the generation that is
//! live, how much of what use left behind was positive and how much was not,
//! compared with the same record for the generation before it. No re-ranking
//! and no retrieval; one aggregate over the observations each one earned.

use crate::core::Core;
use crate::error::Result;
use sqlx::Row;

/// The lived record of one generation.
///
/// `negatives` is a weighted sum rather than a count: a give-up is a quarter
/// of an unsupported literal, and the strengths already say so.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lived {
    pub positives: usize,
    pub negatives: f32,
    pub observations: usize,
}

impl Lived {
    /// Net strength per observation, or `None` where nothing was observed.
    fn rate(&self) -> Option<f64> {
        (self.observations > 0)
            .then(|| (self.positives as f64 - self.negatives as f64) / self.observations as f64)
    }
}

/// Everything observed under `generation_id` that is still usable.
pub async fn lived(core: &Core, generation_id: &str) -> Result<Lived> {
    let r = sqlx::query(
        "SELECT COUNT(*) AS n,
                COALESCE(SUM(CASE WHEN strength > 0 THEN 1 ELSE 0 END), 0) AS positives,
                CAST(COALESCE(SUM(CASE WHEN strength < 0 THEN -strength ELSE 0 END), 0) AS REAL)
                    AS negatives
           FROM observations
          WHERE generation_id = ? AND excluded_at IS NULL",
    )
    .bind(generation_id)
    .fetch_one(&core.store.pool)
    .await?;
    Ok(Lived {
        positives: r.get::<i64, _>("positives") as usize,
        negatives: r.get::<f64, _>("negatives") as f32,
        observations: r.get::<i64, _>("n") as usize,
    })
}

/// The watch gate: whether the adopted generation keeps its place against the
/// one it replaced.
///
/// `recommend` pointed at rates instead of ranks, and deliberately conservative
/// in one direction: the newer generation is kept unless the older one would
/// clear the gate against it. A tie keeps the newer one, because reverting is
/// also a change, and the rule that says a knob moving nothing keeps its value
/// says a generation that lost nothing keeps its place.
///
/// "Clear the gate" means the older rate exceeds the newer by more than a
/// single observation could account for on either side. One strong
/// observation flipping sign moves a record's net by two, so over `n`
/// observations it moves the rate by `2 / n` — and it could have happened in
/// either record. Thin evidence therefore resolves to the branch that changes
/// nothing: one observation cannot separate anything from anything.
pub fn holds_up(new: &Lived, old: &Lived) -> bool {
    let (Some(new_rate), Some(old_rate)) = (new.rate(), old.rate()) else {
        return true;
    };
    let one_observation = 2.0 / new.observations as f64 + 2.0 / old.observations as f64;
    old_rate - new_rate <= one_observation
}

/// Whether the lived evidence has separated the two generations, in either
/// direction — or the newer one has been watched for as long as the record it
/// is measured against.
///
/// What ends a watch. While it runs, nothing new is proposed: one change at a
/// time is what keeps the journal readable and the revert exact. But a watch
/// that never ended would be one adoption and then silence for the life of
/// the base, so it ends when the newer generation would itself clear the gate
/// against the older, or has earned as many observations as the older had
/// when the two were compared. Neither is a number anybody chose: a base in
/// heavy use decides quickly and a quiet one waits.
pub fn settled(new: &Lived, old: &Lived) -> bool {
    let (Some(new_rate), Some(old_rate)) = (new.rate(), old.rate()) else {
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
            positives: 3,
            negatives: 6.0,
            observations: 9,
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
            positives: 5,
            negatives: 2.0,
            observations: 7,
        };
        assert!(holds_up(&a, &a), "reverting is a change too");
    }

    #[test]
    fn too_few_observations_hold_up_rather_than_revert() {
        // Not a floor anybody chose: one observation cannot clear a gate in
        // either direction, and the one that fires on no evidence must be the
        // one that changes nothing.
        let new = Lived {
            positives: 0,
            negatives: 1.0,
            observations: 1,
        };
        let old = Lived {
            positives: 40,
            negatives: 0.0,
            observations: 40,
        };
        assert!(holds_up(&new, &old), "one observation decides nothing");
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
