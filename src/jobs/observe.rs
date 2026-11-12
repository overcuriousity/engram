//! Give-ups: a search nobody opened, followed by another from the same person shortly after, is a
//! weak negative.
//! Read only after `now - window` and only from doors that record opens (`Door::records_opens`).

use crate::core::Core;
use crate::error::Result;
use crate::store::observations::{NewObservation, Source};
use sqlx::Row;

/// A stamp, not a row.
const EVENTS_AFTER: &str = "observe.gave_up_after";

pub async fn run(core: &Core) -> Result<usize> {
    let window = core.evolve.give_up_window_secs;
    if window <= 0 {
        // Off, the watermark still moves with the clock, so switching back on reads nothing from
        // the gap.
        core.store
            .meta_set(EVENTS_AFTER, &crate::store::now().to_string())
            .await?;
        return Ok(0);
    }

    let cutoff = crate::store::now() - window;
    let stamped: Option<i64> = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|s| s.parse().ok());
    let Some(after) = stamped else {
        // The first pass sets the watermark and reads nothing: a backfill would charge every
        // historic unopened search to the live generation.
        core.store
            .meta_set(EVENTS_AFTER, &cutoff.to_string())
            .await?;
        tracing::info!(
            from = cutoff,
            "the give-up watermark starts here; earlier searches are not backfilled"
        );
        return Ok(0);
    };
    if cutoff <= after {
        return Ok(0);
    }

    // Only doors that record opens (`Store::open_event`), and only a named searcher: `Api` and
    // `Mcp` scope is NULL, and a token is not a person.
    let doors: Vec<&'static str> = crate::store::feedback::Door::ALL
        .iter()
        .filter(|d| d.records_opens())
        .map(|d| d.as_str())
        .collect();
    if doors.is_empty() {
        return Ok(0);
    }
    let holes = vec!["?"; doors.len()].join(", ");
    // No generation recorded, nothing honest to charge. Already written searches are skipped: the
    // watermark moves only after the pass.
    let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT e.id AS id, e.query AS query, e.query_vec AS query_vec,
                e.embed_model AS embed_model, e.generation_id AS generation_id
           FROM search_events e
          WHERE e.opened_at IS NULL
            AND e.judged_at IS NULL
            AND e.scope IS NOT NULL
            AND e.generation_id IS NOT NULL
            AND e.door IN ({holes})
            AND e.created_at > ?
            AND e.created_at <= ?
            AND EXISTS (SELECT 1 FROM search_events later
                         WHERE later.id <> e.id
                           AND later.scope = e.scope
                           AND later.created_at > e.created_at
                           AND later.created_at <= e.created_at + ?)
            AND NOT EXISTS (SELECT 1 FROM observations o
                             WHERE o.event_id = e.id AND o.source = ?)
          ORDER BY e.created_at"
    )));
    for d in &doors {
        q = q.bind(*d);
    }
    let rows = q
        .bind(after)
        .bind(cutoff)
        .bind(window)
        .bind(Source::GaveUp.as_str())
        .fetch_all(&core.store.pool)
        .await?;

    // One transaction for the batch; `NOT EXISTS` covers the gap between commit and stamp.
    let mut written = 0;
    let mut tx = core.store.pool.begin().await?;
    for r in &rows {
        // No artifact and no rank: the claim is that the list did not answer.
        crate::store::observations::insert(
            &mut *tx,
            &NewObservation {
                // The generation that drew the list, not the one live now.
                generation_id: r.get("generation_id"),
                query: r.get("query"),
                query_vec: crate::store::feedback::blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                embed_model: r.get("embed_model"),
                artifact_id: None,
                rank: None,
                source: Source::GaveUp,
                event_id: Some(r.get("id")),
            },
        )
        .await?;
        written += 1;
    }
    tx.commit().await?;

    core.store
        .meta_set(EVENTS_AFTER, &cutoff.to_string())
        .await?;
    if written > 0 {
        tracing::info!(written, "searches that were given up on");
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::feedback::{Door, NewCandidate, NewEvent};
    use crate::store::generations::{GenerationParams, NewGeneration};

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
        // Stands for a base past its first pass, which only sets the watermark.
        core.store
            .meta_set(EVENTS_AFTER, &(crate::store::now() - 100_000).to_string())
            .await
            .unwrap();
        (core, generation)
    }

    async fn record_at(core: &Core, query: &str, ago: i64) -> String {
        let id = core
            .store
            .record_search(
                NewEvent {
                    query: query.into(),
                    door: Door::Ui,
                    scope: Some("me".into()),
                    filters: "{}".into(),
                    query_vec: vec![0.1, 0.2, 0.3],
                    embed_model: "fake".into(),
                    candidates: vec![NewCandidate {
                        artifact_id: "art-1".into(),
                        score: 0.9,
                        similarity: Some(0.9),
                        shown: true,
                        ..Default::default()
                    }],
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE search_events SET created_at = ? WHERE id = ?",
            crate::store::now() - ago,
            &id
        )
        .execute(&core.store.pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn the_first_pass_does_not_backfill_the_history() {
        let core = crate::core::test_support::test_core().await;
        let generation = core
            .store
            .record_generation(&NewGeneration {
                params: GenerationParams::default(),
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;

        assert_eq!(run(&core).await.unwrap(), 0, "the history was backfilled");
        assert!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty(),
            "a generation that served none of those searches was charged for them"
        );
        assert!(
            core.store.meta_get(EVENTS_AFTER).await.unwrap().is_some(),
            "without a stamp the next pass backfills after all"
        );

        // Inside the window: an event young enough to gain a successor is not finished.
        record_at(&core, "wie mounte ich", 20).await;
        record_at(&core, "loop mount image", 5).await;
        assert_eq!(run(&core).await.unwrap(), 0, "still inside the window");
    }

    #[tokio::test]
    async fn a_search_nobody_opened_and_then_searched_past_is_a_weak_negative() {
        let (core, generation) = base().await;
        let unopened = record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;

        assert_eq!(run(&core).await.unwrap(), 1);
        let obs = core
            .store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(obs[0].source, Source::GaveUp);
        assert_eq!(obs[0].query, "loop device");
        assert_eq!(
            obs[0].event_id.as_deref(),
            Some(unopened.as_str()),
            "a give-up names the search it is about"
        );
        assert!(
            obs[0].strength < 0.0 && obs[0].strength > -1.0,
            "weak, not strong"
        );
        assert_eq!(obs[0].artifact_id, None, "the list failed, not a row in it");
    }

    #[tokio::test]
    async fn a_search_whose_result_was_opened_is_never_a_give_up() {
        let (core, generation) = base().await;
        let first = record_at(&core, "loop device", 4_000).await;
        core.store.open_event(&first, "art-1").await.unwrap();
        record_at(&core, "mount loop image", 3_940).await;

        run(&core).await.unwrap();
        let obs = core
            .store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert!(obs.iter().all(|o| o.source != Source::GaveUp));
    }

    #[tokio::test]
    async fn a_search_an_hour_later_is_a_new_question_and_not_a_give_up() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 8_000).await;
        record_at(&core, "invoice due date", 8_000 - 3_600).await;

        assert_eq!(run(&core).await.unwrap(), 0);
        assert!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_last_search_of_a_chain_is_not_a_give_up_because_nothing_followed_it() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;
        run(&core).await.unwrap();

        let obs = core
            .store
            .observations_for_generation(&generation, 10)
            .await
            .unwrap();
        assert_eq!(
            obs.len(),
            1,
            "only the abandoned one, never the one that ended it"
        );
    }

    #[tokio::test]
    async fn a_second_pass_writes_nothing_new() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;
        run(&core).await.unwrap();

        assert_eq!(run(&core).await.unwrap(), 0);
        assert_eq!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_search_on_a_door_that_cannot_record_an_open_is_never_a_give_up() {
        let (core, generation) = base().await;
        for (door, scope) in [
            (Door::Api, None),
            (Door::Mcp, None),
            (Door::Cli, Some("me".to_string())),
        ] {
            let id = record_at(&core, "loop device", 4_000).await;
            sqlx::query!(
                "UPDATE search_events SET door = ?, scope = ? WHERE id = ?",
                door.as_str(),
                &scope,
                &id
            )
            .execute(&core.store.pool)
            .await
            .unwrap();
            let next = record_at(&core, "mount loop image", 3_940).await;
            sqlx::query!(
                "UPDATE search_events SET door = ?, scope = ? WHERE id = ?",
                door.as_str(),
                &scope,
                &next
            )
            .execute(&core.store.pool)
            .await
            .unwrap();

            core.store.meta_set(EVENTS_AFTER, "0").await.unwrap();
            assert_eq!(
                run(&core).await.unwrap(),
                0,
                "{} cannot say a list was opened, so it cannot say one was not",
                door.as_str()
            );
        }
        assert!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn switching_the_rule_off_and_on_again_does_not_read_what_happened_while_it_was_off() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;

        let mut core = core;
        core.evolve.give_up_window_secs = 0;
        assert_eq!(run(&core).await.unwrap(), 0);
        core.evolve.give_up_window_secs = 300;
        assert_eq!(
            run(&core).await.unwrap(),
            0,
            "the searches made while the rule was off were read"
        );
        assert!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_give_up_is_charged_to_the_generation_that_served_the_search() {
        let (core, served) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;
        let applied = core
            .store
            .record_generation(&NewGeneration {
                params: GenerationParams {
                    recency_weight: 0.3,
                    ..Default::default()
                },
                embed_recipe: "recipe-a".into(),
                chat_model: "qwen".into(),
                parent_id: Some(served.clone()),
            })
            .await
            .unwrap();

        assert_eq!(run(&core).await.unwrap(), 1);
        assert_eq!(
            core.store
                .observations_for_generation(&served, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            core.store
                .observations_for_generation(&applied, 10)
                .await
                .unwrap()
                .is_empty(),
            "a generation was charged for a list it never drew"
        );
    }

    #[tokio::test]
    async fn a_search_already_written_down_is_not_written_down_twice() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;
        assert_eq!(run(&core).await.unwrap(), 1);

        core.store
            .meta_set(EVENTS_AFTER, &(crate::store::now() - 100_000).to_string())
            .await
            .unwrap();
        assert_eq!(run(&core).await.unwrap(), 0);
        assert_eq!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .len(),
            1,
            "one give-up, counted twice"
        );
    }

    #[tokio::test]
    async fn a_window_of_zero_records_nothing() {
        let (core, generation) = base().await;
        record_at(&core, "loop device", 4_000).await;
        record_at(&core, "mount loop image", 3_940).await;

        let mut core = core;
        core.evolve.give_up_window_secs = 0;
        assert_eq!(run(&core).await.unwrap(), 0);
        assert!(
            core.store
                .observations_for_generation(&generation, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
