//! Whether the evidence use leaves behind still agrees with human verdicts on the same queries:
//! the safeguard that notices the score itself has gone bad. Silence on either side is skipped.

use crate::core::Core;
use crate::error::Result;

/// Judged searches one reading looks at, newest first.
const VERDICT_LIMIT: i64 = 500;

/// How far back a verdict counts: without an age bound, old disagreements suspend the loop forever.
const VERDICT_WINDOW_SECS: i64 = 90 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agreement {
    pub agreed: usize,
    pub disagreed: usize,
}

/// How often the observations and human verdicts agree; `None` where nothing compares.
/// One tally per distinct query, its newest verdict, so one re-judged query cannot dominate.
pub async fn agreement(core: &Core) -> Result<Option<Agreement>> {
    // Scoped to the live models' era: observations made under other models say nothing about these.
    let Some(live) = core.store.live_generation().await? else {
        return Ok(None);
    };
    let verdicts = sqlx::query!(
        r#"SELECT query, verdict AS "verdict!", expect_id FROM (
           SELECT query, verdict, expect_id, judged_at, id,
                  ROW_NUMBER() OVER (PARTITION BY query ORDER BY judged_at DESC, id DESC) AS n
             FROM search_events WHERE verdict IN ('hit', 'gap') AND judged_at >= ?
         ) WHERE n = 1
          ORDER BY judged_at DESC, id DESC LIMIT ?"#,
        crate::store::now() - VERDICT_WINDOW_SECS,
        VERDICT_LIMIT
    )
    .fetch_all(&core.store.pool)
    .await?;

    let mut a = Agreement {
        agreed: 0,
        disagreed: 0,
    };
    let mut any = false;
    for v in verdicts {
        let query: String = v.query;
        let positives: Vec<String> = sqlx::query_scalar!(
            r#"SELECT o.artifact_id AS "artifact_id!" FROM observations o
               JOIN generations g ON g.id = o.generation_id
              WHERE o.query = ? AND o.strength > 0 AND o.artifact_id IS NOT NULL
                AND o.excluded_at IS NULL
                AND g.embed_recipe = ? AND g.chat_model = ?"#,
            &query,
            &live.embed_recipe,
            &live.chat_model
        )
        .fetch_all(&core.store.pool)
        .await?;
        let negative: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM observations o
               JOIN generations g ON g.id = o.generation_id
              WHERE o.query = ? AND o.strength < 0 AND o.excluded_at IS NULL
                AND g.embed_recipe = ? AND g.chat_model = ?",
            &query,
            &live.embed_recipe,
            &live.chat_model
        )
        .fetch_one(&core.store.pool)
        .await?;

        let verdict: String = v.verdict.clone();
        let agreed = match (verdict.as_str(), v.expect_id.clone()) {
            ("hit", Some(expect)) if !positives.is_empty() => {
                // A verdict naming a merged-away artifact is satisfied by what replaced it.
                let satisfies = crate::eval::satisfied_by(core, &expect).await;
                positives.iter().any(|p| satisfies.contains(p))
            }
            ("gap", _) if !positives.is_empty() => false,
            ("gap", _) if negative > 0 => true,
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

/// Trusted until no better than chance on more disagreements than noise accounts for.
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
    async fn a_query_judged_many_times_is_still_one_piece_of_evidence() {
        let (core, g) = base().await;
        observed(&core, &g, "mount the image", Some("art-1"), Source::Cited).await;
        for _ in 0..30 {
            judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        }
        judged(&core, "loop device", Verdict::Hit, Some("art-2")).await;
        observed(&core, &g, "loop device", Some("art-9"), Source::Opened).await;

        assert_eq!(
            agreement(&core).await.unwrap(),
            Some(Agreement {
                agreed: 1,
                disagreed: 1
            }),
            "one tally per query, not one per verdict row"
        );
    }

    #[tokio::test]
    async fn the_newest_verdict_on_a_query_is_the_one_that_counts() {
        let (core, g) = base().await;
        observed(&core, &g, "mount the image", Some("art-1"), Source::Cited).await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-9")).await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        assert_eq!(
            agreement(&core).await.unwrap(),
            Some(Agreement {
                agreed: 1,
                disagreed: 0
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
    async fn an_observation_from_another_era_is_not_what_a_verdict_is_weighed_against() {
        let (core, g) = base().await;
        observed(&core, &g, "mount the image", Some("art-9"), Source::Cited).await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        assert_eq!(
            agreement(&core).await.unwrap(),
            Some(Agreement {
                agreed: 0,
                disagreed: 1
            }),
            "this era's own observation disagrees"
        );

        // The models change: a new era, and the old observation belongs to the one that ended.
        core.store
            .record_generation(&NewGeneration {
                params: GenerationParams::default(),
                embed_recipe: "recipe-b".into(),
                chat_model: "qwen".into(),
                parent_id: Some(g),
            })
            .await
            .unwrap();
        assert_eq!(
            agreement(&core).await.unwrap(),
            None,
            "nothing this era observed about that query"
        );
    }

    #[tokio::test]
    async fn a_verdict_older_than_the_window_is_not_read() {
        let (core, g) = base().await;
        observed(&core, &g, "mount the image", Some("art-9"), Source::Cited).await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        assert!(agreement(&core).await.unwrap().is_some());

        sqlx::query!(
            "UPDATE search_events SET judged_at = ?",
            crate::store::now() - VERDICT_WINDOW_SECS - 1
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        assert_eq!(agreement(&core).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_verdict_nobody_observed_anything_about_is_not_in_the_sample() {
        let (core, g) = base().await;
        judged(&core, "mount the image", Verdict::Hit, Some("art-1")).await;
        assert_eq!(agreement(&core).await.unwrap(), None);
        observed(&core, &g, "another question", Some("art-1"), Source::Cited).await;
        assert_eq!(agreement(&core).await.unwrap(), None);
    }
}
