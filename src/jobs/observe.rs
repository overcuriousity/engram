//! The one signal a plain search gives up on its own.
//!
//! A search nobody opened, followed by another search from the same person a
//! short while later, is a search that did not answer. It is the only negative
//! the search door produces without being asked, and it is deliberately weak:
//! the rail shows snippets, so a search read and walked away from *satisfied*
//! looks exactly like one given up on. What separates them is not time or
//! attention but the second attempt — a failed recall is what issues another
//! cue.
//!
//! `fold_onto` is not this signal and must not be read as one. It coalesces a
//! typing burst into one event and overwrites the wordings inside it on
//! purpose: what survives is the query that was actually meant. What it buys
//! here is that consecutive stored events are already distinct search acts
//! rather than keystrokes.
//!
//! Idempotent by watermark, the way `associate` reads the same log: a pass
//! considers events after the last stamp and no later than `now - window`,
//! because an event young enough to still gain a successor has not finished
//! being what it is.

use crate::core::Core;
use crate::error::Result;
use crate::store::observations::{NewObservation, Source};
use sqlx::Row;

/// Where the last pass stopped. A stamp, not a row.
const EVENTS_AFTER: &str = "observe.gave_up_after";

pub async fn run(core: &Core) -> Result<usize> {
    let window = core.evolve.give_up_window_secs;
    if window <= 0 {
        return Ok(0);
    }
    let Some(generation) = core.store.live_generation().await? else {
        // The boot path that names a generation runs in the background. Until
        // it has, there is nothing for an observation to be evidence about.
        return Ok(0);
    };

    let after: i64 = core
        .store
        .meta_get(EVENTS_AFTER)
        .await?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let cutoff = crate::store::now() - window;
    if cutoff <= after {
        return Ok(0);
    }

    let rows = sqlx::query(
        "SELECT e.id AS id, e.query AS query, e.query_vec AS query_vec,
                e.embed_model AS embed_model
           FROM search_events e
          WHERE e.opened_at IS NULL
            AND e.judged_at IS NULL
            AND e.created_at > ?
            AND e.created_at <= ?
            AND EXISTS (SELECT 1 FROM search_events later
                         WHERE later.id <> e.id
                           AND later.scope IS e.scope
                           AND later.created_at > e.created_at
                           AND later.created_at <= e.created_at + ?)
          ORDER BY e.created_at",
    )
    .bind(after)
    .bind(cutoff)
    .bind(window)
    .fetch_all(&core.store.pool)
    .await?;

    let mut written = 0;
    for r in &rows {
        // No artifact and no rank: the claim is that the list did not answer,
        // not that anything in it was wrong to be there.
        core.store
            .record_observation(&NewObservation {
                generation_id: generation.id.clone(),
                query: r.get("query"),
                query_vec: crate::store::feedback::blob_to_vec(&r.get::<Vec<u8>, _>("query_vec")),
                embed_model: r.get("embed_model"),
                artifact_id: None,
                rank: None,
                source: Source::GaveUp,
                event_id: Some(r.get("id")),
            })
            .await?;
        written += 1;
    }

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
        (core, generation)
    }

    /// A search recorded and then backdated, so a chain can be built without
    /// waiting for one.
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
                        band: false,
                    }],
                    answered: false,
                    fold_onto: None,
                    context: None,
                },
                0,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE search_events SET created_at = ? WHERE id = ?")
            .bind(crate::store::now() - ago)
            .bind(&id)
            .execute(&core.store.pool)
            .await
            .unwrap();
        id
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
        // Search, read something, leave the page open, come back to something
        // else entirely. The window is what stops that being scored as the
        // failure of a search that worked.
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
