//! Whether the evidence use leaves behind still agrees with the people using
//! the base.
//!
//! What the idle pass optimises is *the generator and the operator could use
//! what retrieval gave them*, which is one step removed from *the person got
//! their answer*. Human verdicts are the sparse, honest sample that checks the
//! two have not come apart. They are never used for volume; they are the
//! anchor, and this is the one safeguard the rest of the loop leans on.
//! Everything else is recoverable by a revert. This is the thing that notices
//! the score itself has gone bad.
//!
//! Agreement is read over searches carrying both a verdict and an observation
//! about the same query. A search judged a hit agrees when a positive
//! observation names the artifact the verdict named (or what superseded it),
//! and disagrees when the positives name something else. A search judged a
//! gap disagrees when anything positive was observed for it, and agrees when
//! only a negative was. Silence on either side is not in the sample.

use crate::core::Core;
use crate::error::Result;
use sqlx::Row;

/// How many judged searches one reading looks at. A bound on work, newest
/// first: the question is whether agreement holds *now*.
const VERDICT_LIMIT: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agreement {
    pub agreed: usize,
    pub disagreed: usize,
}

/// How often the self-generated evidence and the human verdicts say the same
/// thing. `None` where no judged search has an observation to compare with.
pub async fn agreement(core: &Core) -> Result<Option<Agreement>> {
    let verdicts = sqlx::query(
        "SELECT query, verdict, expect_id FROM search_events
          WHERE verdict IN ('hit', 'gap')
          ORDER BY judged_at DESC, id DESC LIMIT ?",
    )
    .bind(VERDICT_LIMIT)
    .fetch_all(&core.store.pool)
    .await?;

    let mut a = Agreement {
        agreed: 0,
        disagreed: 0,
    };
    let mut any = false;
    for v in &verdicts {
        let query: String = v.get("query");
        let positives: Vec<String> = sqlx::query_scalar(
            "SELECT artifact_id FROM observations
              WHERE query = ? AND strength > 0 AND artifact_id IS NOT NULL
                AND excluded_at IS NULL",
        )
        .bind(&query)
        .fetch_all(&core.store.pool)
        .await?;
        let negative: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observations
              WHERE query = ? AND strength < 0 AND excluded_at IS NULL",
        )
        .bind(&query)
        .fetch_one(&core.store.pool)
        .await?;

        let verdict: String = v.get("verdict");
        let agreed = match (verdict.as_str(), v.get::<Option<String>, _>("expect_id")) {
            ("hit", Some(expect)) if !positives.is_empty() => {
                // The supersede rule, unchanged: a verdict naming an artifact
                // that was merged away is satisfied by what replaced it.
                let satisfies = crate::eval::satisfied_by(core, &expect).await;
                positives.iter().any(|p| satisfies.contains(p))
            }
            ("gap", _) if !positives.is_empty() => false,
            ("gap", _) if negative > 0 => true,
            // A verdict with nothing observed beside it compares with nothing.
            _ => continue,
        };
        any = true;
        if agreed {
            a.agreed += 1;
        } else {
            a.disagreed += 1;
        }
    }
    Ok(any.then_some(a))
}

/// Whether the self-generated evidence can be trusted to move anything.
///
/// Not a tuned threshold. Agreement is trusted until it is shown to be no
/// better than chance — as many disagreements as agreements — on more
/// disagreements than one could account for. The same shape as every other
/// gate here: one disagreement is noise wearing a verdict, and thin evidence
/// must not suspend a base any more than it may move one.
pub fn trustworthy(a: &Agreement) -> bool {
    a.disagreed < 2 || a.agreed > a.disagreed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::feedback::{Door, Labeller, NewEvent, Verdict};
    use crate::store::generations::{GenerationParams, NewGeneration};
    use crate::store::observations::{NewObservation, Source};

    #[test]
    fn agreement_that_beats_chance_is_trustworthy() {
        assert!(trustworthy(&Agreement {
            agreed: 18,
            disagreed: 2
        }));
    }

    #[test]
    fn agreement_no_better_than_chance_is_not() {
        assert!(!trustworthy(&Agreement {
            agreed: 10,
            disagreed: 10
        }));
        assert!(!trustworthy(&Agreement {
            agreed: 0,
            disagreed: 2
        }));
    }

    #[test]
    fn two_verdicts_decide_nothing_either_way() {
        assert!(
            trustworthy(&Agreement {
                agreed: 2,
                disagreed: 0
            }),
            "thin evidence must not suspend a base any more than it moves one"
        );
        assert!(
            trustworthy(&Agreement {
                agreed: 0,
                disagreed: 1
            }),
            "one disagreement is noise wearing a verdict"
        );
        assert!(trustworthy(&Agreement {
            agreed: 1,
            disagreed: 1
        }));
    }

    async fn base() -> (Core, String) {
        let core = crate::core::test_support::test_core().await;
        let generation = core
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
        (core, generation)
    }

    async fn judged(core: &Core, query: &str, verdict: Verdict, expect: Option<&str>) {
        let id = core
            .store
            .record_search(
                NewEvent {
                    fold_onto: None,
                    query: query.into(),
                    door: Door::Ui,
                    scope: None,
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2],
                    embed_model: "fake".into(),
                    candidates: vec![],
                    answered: false,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        match expect {
            Some(e) => core.store.judge_hit(&id, e, Labeller::Deck).await.unwrap(),
            None => core
                .store
                .judge(&id, verdict, Labeller::Deck)
                .await
                .unwrap(),
        };
    }

    async fn observed(
        core: &Core,
        generation: &str,
        query: &str,
        artifact: Option<&str>,
        source: Source,
    ) {
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.to_string(),
                query: query.into(),
                query_vec: vec![0.1, 0.2],
                embed_model: "fake".into(),
                artifact_id: artifact.map(str::to_string),
                rank: artifact.map(|_| 1),
                source,
                event_id: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_hit_agrees_when_the_observation_names_what_the_verdict_named() {
        let (core, g) = base().await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        observed(&core, &g, "mount the image", Some("art-1"), Source::Cited).await;
        judged(&core, "loop device", Verdict::Hit, Some("art-2")).await;
        observed(&core, &g, "loop device", Some("art-9"), Source::Opened).await;

        assert_eq!(
            agreement(&core).await.unwrap(),
            Some(Agreement {
                agreed: 1,
                disagreed: 1
            })
        );
    }

    #[tokio::test]
    async fn a_gap_disagrees_with_anything_positive_and_agrees_with_a_give_up() {
        let (core, g) = base().await;
        judged(&core, "nothing here", Verdict::Gap, None).await;
        observed(&core, &g, "nothing here", Some("art-1"), Source::Cited).await;
        judged(&core, "nor here", Verdict::Gap, None).await;
        observed(&core, &g, "nor here", None, Source::GaveUp).await;

        assert_eq!(
            agreement(&core).await.unwrap(),
            Some(Agreement {
                agreed: 1,
                disagreed: 1
            })
        );
    }

    #[tokio::test]
    async fn a_verdict_nobody_observed_anything_about_is_not_in_the_sample() {
        let (core, g) = base().await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        assert_eq!(agreement(&core).await.unwrap(), None);
        // And an observation about some other query does not make it one.
        observed(&core, &g, "another question", Some("art-1"), Source::Cited).await;
        assert_eq!(agreement(&core).await.unwrap(), None);
    }
}
